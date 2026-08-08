use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use libvips::{VipsApp, VipsImage, ops};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::env;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const APP_ID: &str = "dev.aethera.BetterImageView";
const DEFAULT_CACHE_MB: usize = 512;
const DECODE_WIDTH: i32 = 2560;
const DECODE_HEIGHT: i32 = 1600;
const ZOOM_LEVELS: &[f64] = &[
    0.125, 0.167, 0.25, 0.333, 0.5, 0.667, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0,
    10.0, 12.0, 16.0, 20.0, 24.0, 32.0,
];
const PRINT_MARGIN_MM: f64 = 12.0;
#[cfg(any())]
const PRINT_RASTER_DPI: f64 = 300.0;

#[derive(Clone, Copy)]
struct PrintSpec {
    paper_width_mm: f64,
    paper_height_mm: f64,
    fit_to_page: bool,
    image_dpi: f64,
    // Placement inside the printable area: 0.0 = left/top edge, 0.5 = centered,
    // 1.0 = right/bottom edge, plus a free offset in millimetres.
    align_x: f64,
    align_y: f64,
    offset_x_mm: f64,
    offset_y_mm: f64,
}

impl PrintSpec {
    fn image_rect_points(&self, image_width: i32, image_height: i32) -> (f64, f64, f64, f64) {
        let margin = PRINT_MARGIN_MM / 25.4 * 72.0;
        let paper_width = self.paper_width_mm / 25.4 * 72.0;
        let paper_height = self.paper_height_mm / 25.4 * 72.0;
        let available_width = paper_width - margin * 2.0;
        let available_height = paper_height - margin * 2.0;
        let (width, height) = if self.fit_to_page {
            let scale =
                (available_width / image_width as f64).min(available_height / image_height as f64);
            (image_width as f64 * scale, image_height as f64 * scale)
        } else {
            (
                image_width as f64 / self.image_dpi * 72.0,
                image_height as f64 / self.image_dpi * 72.0,
            )
        };
        (
            margin + self.align_x * (available_width - width) + self.offset_x_mm / 25.4 * 72.0,
            margin + self.align_y * (available_height - height) + self.offset_y_mm / 25.4 * 72.0,
            width,
            height,
        )
    }
}

fn default_zoom(view_width: i32, view_height: i32, image_width: i32, image_height: i32) -> f64 {
    (view_width as f64 / image_width as f64)
        .min(view_height as f64 / image_height as f64)
        .min(1.0)
}

fn scaled_image_size(image_width: i32, image_height: i32, zoom: f64) -> (f32, f32) {
    (
        image_width as f32 * zoom as f32,
        image_height as f32 * zoom as f32,
    )
}

fn next_zoom_level(current: f64) -> f64 {
    ZOOM_LEVELS
        .iter()
        .copied()
        .find(|level| *level > current + f64::EPSILON)
        .unwrap_or(*ZOOM_LEVELS.last().expect("zoom levels must not be empty"))
}

fn previous_zoom_level(current: f64, fit: f64) -> Option<f64> {
    let previous = ZOOM_LEVELS
        .iter()
        .rev()
        .copied()
        .find(|level| *level < current - f64::EPSILON);
    match previous {
        Some(level) if level > fit => Some(level),
        _ => None,
    }
}

fn format_zoom(zoom: f64) -> String {
    let percentage = zoom * 100.0;
    if (percentage - percentage.round()).abs() < 0.01 {
        format!("{percentage:.0}%")
    } else {
        format!("{percentage:.1}%")
    }
}

mod canvas_imp {
    use super::*;

    #[derive(Default)]
    pub struct ImageCanvas {
        pub texture: RefCell<Option<gdk::Texture>>,
        pub zoom: Cell<f64>,
        pub fit_zoom: Cell<f64>,
        pub source_width: Cell<i32>,
        pub source_height: Cell<i32>,
        pub decoded_scale: Cell<f64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ImageCanvas {
        const NAME: &'static str = "BetterImageViewCanvas";
        type Type = super::ImageCanvas;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for ImageCanvas {}

    impl WidgetImpl for ImageCanvas {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let Some(texture) = self.texture.borrow().as_ref().cloned() else {
                return (0, 0, -1, -1);
            };
            let zoom = self.zoom.get();
            if zoom <= 0.0 {
                return (0, 0, -1, -1);
            }
            let size = if orientation == gtk::Orientation::Horizontal {
                texture.width()
            } else {
                texture.height()
            };
            let size = (size as f64 * zoom).round() as i32;
            (size, size, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let Some(texture) = self.texture.borrow().as_ref().cloned() else {
                return;
            };
            let widget = self.obj();
            let available_width = widget.width() as f32;
            let available_height = widget.height() as f32;
            let zoom = if self.zoom.get() <= 0.0 {
                default_zoom(
                    widget.width(),
                    widget.height(),
                    texture.width(),
                    texture.height(),
                )
            } else {
                self.zoom.get()
            };
            // The scrolled window can briefly allocate the two axes at different
            // points during relayout. Never use that allocation as the image's
            // draw size: one zoom factor must govern both axes so the texture
            // cannot stretch while zooming.
            let (width, height) = scaled_image_size(texture.width(), texture.height(), zoom);
            let bounds = gtk::graphene::Rect::new(
                (available_width - width) / 2.0,
                (available_height - height) / 2.0,
                width,
                height,
            );
            let filter = if self.zoom.get() > 1.0 {
                gtk::gsk::ScalingFilter::Nearest
            } else {
                gtk::gsk::ScalingFilter::Linear
            };
            snapshot.append_scaled_texture(&texture, filter, &bounds);
        }
    }
}

glib::wrapper! {
    pub struct ImageCanvas(ObjectSubclass<canvas_imp::ImageCanvas>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ImageCanvas {
    fn new() -> Self {
        glib::Object::builder()
            .property("hexpand", true)
            .property("vexpand", true)
            .build()
    }

    fn set_texture(&self, texture: &impl IsA<gdk::Texture>, source_width: i32, source_height: i32) {
        let texture = texture.as_ref();
        self.imp().texture.replace(Some(texture.clone()));
        self.imp().zoom.set(0.0);
        self.imp().fit_zoom.set(0.0);
        self.imp().source_width.set(source_width);
        self.imp().source_height.set(source_height);
        self.imp()
            .decoded_scale
            .set(texture.width() as f64 / source_width as f64);
        self.queue_resize();
        self.queue_draw();
    }

    fn zoom_in(&self) {
        let imp = self.imp();
        if imp.texture.borrow().is_none() {
            return;
        }
        let current = imp.zoom.get();
        let fit = default_zoom(
            self.width(),
            self.height(),
            imp.source_width.get(),
            imp.source_height.get(),
        );
        let decoded_scale = imp.decoded_scale.get();
        let current_source_zoom = current * decoded_scale;
        let target = if current <= 0.0 {
            imp.fit_zoom.set(fit);
            next_zoom_level(fit)
        } else {
            next_zoom_level(current_source_zoom)
        };
        imp.zoom.set(target / decoded_scale);
        self.queue_resize();
    }

    fn zoom_out(&self) {
        let imp = self.imp();
        if imp.texture.borrow().is_none() {
            return;
        }
        let fit = imp.fit_zoom.get();
        let decoded_scale = imp.decoded_scale.get();
        let current = imp.zoom.get() * decoded_scale;
        imp.zoom.set(
            previous_zoom_level(current, fit)
                .map(|level| level / decoded_scale)
                .unwrap_or(0.0),
        );
        self.queue_resize();
    }

    fn zoom_fit(&self) {
        self.imp().zoom.set(0.0);
        self.imp().fit_zoom.set(0.0);
        self.queue_resize();
    }

    fn is_zoomed(&self) -> bool {
        self.imp().zoom.get() > 0.0
    }

    fn effective_zoom(&self) -> Option<f64> {
        let imp = self.imp();
        let texture = imp.texture.borrow();
        texture.as_ref()?;
        let zoom = imp.zoom.get();
        Some(if zoom > 0.0 {
            zoom * imp.decoded_scale.get()
        } else {
            default_zoom(
                self.width(),
                self.height(),
                imp.source_width.get(),
                imp.source_height.get(),
            )
        })
    }

    fn texture(&self) -> Option<gdk::Texture> {
        self.imp().texture.borrow().clone()
    }
}

#[derive(Debug)]
enum LoaderCommand {
    Batch {
        generation: u64,
        paths: Vec<PathBuf>,
    },
    Stop,
}

#[derive(Debug)]
struct DecodedFrame {
    path: PathBuf,
    width: i32,
    height: i32,
    stride: usize,
    pixels: Arc<[u8]>,
    source_width: i32,
    source_height: i32,
    source_dpi: Option<f64>,
    file_type: String,
    metadata: String,
}

impl DecodedFrame {
    fn size_bytes(&self) -> usize {
        self.pixels.len()
    }
}

#[derive(Debug)]
enum LoaderEvent {
    Loaded {
        generation: u64,
        frame: DecodedFrame,
    },
    Failed {
        generation: u64,
        path: PathBuf,
        message: String,
    },
}

struct FrameCache {
    limit_bytes: usize,
    used_bytes: usize,
    frames: HashMap<PathBuf, Arc<DecodedFrame>>,
    lru: VecDeque<PathBuf>,
}

impl FrameCache {
    fn new(limit_bytes: usize) -> Self {
        Self {
            limit_bytes,
            used_bytes: 0,
            frames: HashMap::new(),
            lru: VecDeque::new(),
        }
    }

    fn get(&mut self, path: &Path) -> Option<Arc<DecodedFrame>> {
        let frame = self.frames.get(path).cloned()?;
        self.touch(path);
        Some(frame)
    }

    fn contains(&self, path: &Path) -> bool {
        self.frames.contains_key(path)
    }

    fn insert(&mut self, frame: DecodedFrame, protected: &Path) {
        let path = frame.path.clone();
        let size = frame.size_bytes();
        if let Some(old) = self.frames.insert(path.clone(), Arc::new(frame)) {
            self.used_bytes = self.used_bytes.saturating_sub(old.size_bytes());
        }
        self.used_bytes += size;
        self.touch(&path);

        while self.used_bytes > self.limit_bytes && self.frames.len() > 1 {
            let Some(candidate) = self.lru.pop_front() else {
                break;
            };
            if candidate == protected {
                self.lru.push_back(candidate);
                continue;
            }
            if let Some(removed) = self.frames.remove(&candidate) {
                self.used_bytes = self.used_bytes.saturating_sub(removed.size_bytes());
            }
        }
    }

    fn remove(&mut self, path: &Path) {
        if let Some(removed) = self.frames.remove(path) {
            self.used_bytes = self.used_bytes.saturating_sub(removed.size_bytes());
        }
        self.lru.retain(|candidate| candidate != path);
    }

    fn touch(&mut self, path: &Path) {
        self.lru.retain(|candidate| candidate != path);
        self.lru.push_back(path.to_path_buf());
    }
}

struct Viewer {
    paths: Vec<PathBuf>,
    index: usize,
    direction: i32,
    generation: u64,
    canvas: ImageCanvas,
    scroller: gtk::ScrolledWindow,
    status: gtk::Label,
    info: gtk::Label,
    metadata: gtk::Label,
    metadata_panel: gtk::Revealer,
    edit_panel: gtk::Revealer,
    edit_rotation: Cell<u8>,
    resize_edge: gtk::SpinButton,
    edit_dimensions: gtk::Label,
    window: gtk::ApplicationWindow,
    cache: FrameCache,
    loader_tx: mpsc::Sender<LoaderCommand>,
    skip_delete_confirmation: Cell<bool>,
}

impl Viewer {
    fn current_path(&self) -> &Path {
        &self.paths[self.index]
    }

    fn move_by(&mut self, delta: i32) {
        let next = (self.index as i32 + delta).clamp(0, self.paths.len() as i32 - 1);
        if next as usize == self.index {
            return;
        }
        self.direction = delta.signum();
        self.index = next as usize;
        self.generation += 1;
        self.show_or_load();
    }

    fn move_to(&mut self, index: usize) {
        if index == self.index {
            return;
        }
        self.direction = if index > self.index { 1 } else { -1 };
        self.index = index.min(self.paths.len() - 1);
        self.generation += 1;
        self.show_or_load();
    }

    fn show_or_load(&mut self) {
        let current = self.current_path().to_path_buf();
        self.edit_rotation.set(0);
        self.update_title();
        if let Some(frame) = self.cache.get(&current) {
            self.resize_edge
                .set_range(1.0, frame.source_width.max(frame.source_height) as f64);
            self.resize_edge
                .set_value(frame.source_width.max(frame.source_height) as f64);
            self.present_frame(&frame);
            self.status.set_text("");
        } else {
            self.status.set_text("Loading…");
        }
        self.request_prefetch();
    }

    fn update_title(&self) {
        let name = self
            .current_path()
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("image");
        self.window.set_title(Some(&format!(
            "{name}  —  {} / {}",
            self.index + 1,
            self.paths.len()
        )));
    }

    fn toggle_fullscreen(&self) {
        if self.window.is_fullscreen() {
            self.window.unfullscreen();
        } else {
            self.window.fullscreen();
        }
    }

    fn toggle_info(&self) {
        self.update_info();
        self.info.set_visible(!self.info.is_visible());
    }

    fn update_info(&self) {
        let Some(frame) = self.cache.frames.get(self.current_path()) else {
            return;
        };
        let name = frame
            .path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("image");
        let zoom = format_zoom(self.canvas.effective_zoom().unwrap_or(1.0));
        self.info.set_text(&format!(
            "{name}\n{} × {} · {} · {zoom}",
            frame.source_width, frame.source_height, frame.file_type
        ));
    }

    fn zoom_in(&self) {
        self.canvas.zoom_in();
        self.update_info();
    }

    fn zoom_out(&self) {
        self.canvas.zoom_out();
        self.update_info();
    }

    fn zoom_fit(&self) {
        self.canvas.zoom_fit();
        self.update_info();
    }

    fn toggle_metadata(&self) {
        let reveal = !self.metadata_panel.reveals_child();
        self.metadata_panel.set_reveal_child(reveal);
        if reveal {
            self.edit_panel.set_reveal_child(false);
        }
    }

    fn toggle_edit(&self) {
        let reveal = !self.edit_panel.reveals_child();
        self.edit_panel.set_reveal_child(reveal);
        if reveal {
            self.metadata_panel.set_reveal_child(false);
            self.reset_edits();
        }
    }

    fn reset_edits(&self) {
        self.edit_rotation.set(0);
        if let Some(frame) = self.cache.frames.get(self.current_path()) {
            self.resize_edge
                .set_range(1.0, frame.source_width.max(frame.source_height) as f64);
            self.resize_edge
                .set_value(frame.source_width.max(frame.source_height) as f64);
            self.present_frame(frame);
            self.update_edit_dimensions(frame);
        }
    }

    fn rotate_edit(&self, delta: i8) {
        let rotation = (self.edit_rotation.get() as i8 + delta).rem_euclid(4) as u8;
        self.edit_rotation.set(rotation);
        if let Some(frame) = self.cache.frames.get(self.current_path()) {
            self.present_frame(frame);
            self.update_edit_dimensions(frame);
        }
    }

    fn update_edit_dimensions(&self, frame: &DecodedFrame) {
        let (width, height) = edited_dimensions(
            frame.source_width,
            frame.source_height,
            self.edit_rotation.get(),
            self.resize_edge.value() as i32,
        );
        self.edit_dimensions
            .set_text(&format!("Output: {width} × {height}"));
    }

    fn open_with(&self) {
        let file = gtk::gio::File::for_path(self.current_path());
        let launcher = gtk::FileLauncher::new(Some(&file));
        launcher.set_always_ask(true);
        let status = self.status.clone();
        launcher.launch(
            Some(&self.window),
            gtk::gio::Cancellable::NONE,
            move |result| {
                if let Err(error) = result {
                    status.set_text(&format!("Could not open application chooser: {error}"));
                }
            },
        );
    }

    fn copy_bitmap(&self) {
        if let Some(texture) = self.canvas.texture() {
            gdk::Display::default()
                .expect("GTK display should be available")
                .clipboard()
                .set_texture(&texture);
            self.status.set_text("Bitmap copied");
        }
    }

    fn copy_filename(&self) {
        let name = self
            .current_path()
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("image");
        gdk::Display::default()
            .expect("GTK display should be available")
            .clipboard()
            .set_text(name);
        self.status.set_text("Filename copied");
    }

    fn print(&self) {
        let Some(frame) = self.cache.frames.get(self.current_path()).cloned() else {
            self.status.set_text("Wait for the image to finish loading");
            return;
        };
        run_print_dialog(&self.window, &self.status, frame);
    }

    fn pan_by(&self, dx: f64, dy: f64) {
        let horizontal = self.scroller.hadjustment();
        let vertical = self.scroller.vadjustment();
        horizontal.set_value((horizontal.value() + dx).clamp(
            horizontal.lower(),
            horizontal.upper() - horizontal.page_size(),
        ));
        vertical.set_value(
            (vertical.value() + dy)
                .clamp(vertical.lower(), vertical.upper() - vertical.page_size()),
        );
    }

    fn forget_current_image(&mut self) {
        let removed = self.paths.remove(self.index);
        self.cache.remove(&removed);
        if self.paths.is_empty() {
            self.window.close();
            return;
        }
        self.index = self.index.min(self.paths.len() - 1);
        self.generation += 1;
        self.show_or_load();
    }

    fn request_prefetch(&self) {
        let mut requested = vec![self.current_path().to_path_buf()];
        let offsets: &[i32] = if self.direction >= 0 {
            &[1, 2, -1]
        } else {
            &[-1, -2, 1]
        };
        for offset in offsets {
            let candidate = self.index as i32 + offset;
            if candidate >= 0 && candidate < self.paths.len() as i32 {
                let path = self.paths[candidate as usize].clone();
                if !self.cache.contains(&path) {
                    requested.push(path);
                }
            }
        }
        requested.dedup();
        let _ = self.loader_tx.send(LoaderCommand::Batch {
            generation: self.generation,
            paths: requested,
        });
    }

    fn accept(&mut self, event: LoaderEvent) {
        match event {
            LoaderEvent::Loaded { generation, frame } => {
                let is_current = frame.path == self.current_path();
                let current = self.current_path().to_path_buf();
                self.cache.insert(frame, &current);
                if is_current && generation == self.generation {
                    if let Some(frame) = self.cache.get(&current) {
                        self.resize_edge
                            .set_range(1.0, frame.source_width.max(frame.source_height) as f64);
                        self.resize_edge
                            .set_value(frame.source_width.max(frame.source_height) as f64);
                        self.present_frame(&frame);
                        self.status.set_text("");
                    }
                }
            }
            LoaderEvent::Failed {
                generation,
                path,
                message,
            } => {
                if generation == self.generation && path == self.current_path() {
                    self.status.set_text(&format!("Could not load: {message}"));
                }
            }
        }
    }

    fn present_frame(&self, frame: &DecodedFrame) {
        let rotation = self.edit_rotation.get();
        let texture = texture_for_rotation(frame, rotation);
        let (source_width, source_height) = if rotation % 2 == 0 {
            (frame.source_width, frame.source_height)
        } else {
            (frame.source_height, frame.source_width)
        };
        self.canvas
            .set_texture(&texture, source_width, source_height);
        self.update_info();
        self.metadata.set_text(&frame.metadata);
        if self.edit_panel.reveals_child() {
            self.update_edit_dimensions(frame);
        }
    }
}

fn edited_dimensions(
    source_width: i32,
    source_height: i32,
    rotation: u8,
    maximum_edge: i32,
) -> (i32, i32) {
    let (width, height) = if rotation % 2 == 0 {
        (source_width, source_height)
    } else {
        (source_height, source_width)
    };
    let edge = width.max(height);
    if maximum_edge <= 0 || maximum_edge >= edge {
        return (width, height);
    }
    let scale = maximum_edge as f64 / edge as f64;
    (
        (width as f64 * scale).round().max(1.0) as i32,
        (height as f64 * scale).round().max(1.0) as i32,
    )
}

fn texture_for_rotation(frame: &DecodedFrame, rotation: u8) -> gdk::MemoryTexture {
    let rotation = rotation % 4;
    if rotation == 0 {
        let bytes = glib::Bytes::from_owned(frame.pixels.clone());
        return gdk::MemoryTexture::new(
            frame.width,
            frame.height,
            gdk::MemoryFormat::R8g8b8a8,
            &bytes,
            frame.stride,
        );
    }

    let (width, height) = if rotation % 2 == 0 {
        (frame.width, frame.height)
    } else {
        (frame.height, frame.width)
    };
    let mut pixels = vec![0_u8; width as usize * height as usize * 4];
    for source_y in 0..frame.height {
        for source_x in 0..frame.width {
            let (target_x, target_y) = match rotation {
                1 => (frame.height - 1 - source_y, source_x),
                2 => (frame.width - 1 - source_x, frame.height - 1 - source_y),
                _ => (source_y, frame.width - 1 - source_x),
            };
            let source = source_y as usize * frame.stride + source_x as usize * 4;
            let target = (target_y as usize * width as usize + target_x as usize) * 4;
            pixels[target..target + 4].copy_from_slice(&frame.pixels[source..source + 4]);
        }
    }
    let bytes = glib::Bytes::from_owned(pixels);
    gdk::MemoryTexture::new(
        width,
        height,
        gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        width as usize * 4,
    )
}

fn main() -> glib::ExitCode {
    // GTK's Vulkan renderer repeatedly reports VK_SUBOPTIMAL_KHR when replacing
    // image textures on some Linux graphics stacks. Prefer OpenGL for this viewer,
    // while preserving an explicit renderer choice made by the caller.
    if env::var_os("GSK_RENDERER").is_none() {
        // SAFETY: this runs before GTK is initialized or any worker threads exist.
        unsafe {
            env::set_var("GSK_RENDERER", "gl");
        }
    }

    let Some(initial_path) = env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: better-image-view IMAGE");
        return glib::ExitCode::FAILURE;
    };

    let initial_path = match initial_path.canonicalize() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("cannot open {}: {error}", initial_path.display());
            return glib::ExitCode::FAILURE;
        }
    };

    let paths = match sibling_images(&initial_path) {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("{error}");
            return glib::ExitCode::FAILURE;
        }
    };
    let Some(index) = paths.iter().position(|path| path == &initial_path) else {
        eprintln!("initial image disappeared while scanning its directory");
        return glib::ExitCode::FAILURE;
    };

    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| {
        build_ui(app, paths.clone(), index);
    });
    // The image argument is handled above. Do not pass it to GApplication again:
    // without HANDLES_OPEN, GTK interprets it as a file-open request and aborts.
    app.run_with_args::<&str>(&[])
}

fn build_ui(app: &gtk::Application, paths: Vec<PathBuf>, index: usize) {
    let css = gtk::CssProvider::new();
    css.load_from_string(
        "
        button.delete-confirm,
        button.delete-confirm:hover,
        button.delete-confirm:active,
        button.delete-confirm:focus {
            color: white;
            background: #7f1d1d;
        }
        .edit-panel {
            background: rgba(24, 24, 24, 0.96);
        }
        ",
    );
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("GTK display should be available"),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let canvas = ImageCanvas::new();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&canvas)
        .build();
    let status = gtk::Label::builder()
        .halign(gtk::Align::Center)
        .valign(gtk::Align::End)
        .margin_bottom(18)
        .build();
    status.add_css_class("title-3");
    let info = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Start)
        .margin_top(18)
        .margin_start(18)
        .xalign(0.0)
        .visible(false)
        .build();
    info.add_css_class("osd");
    let metadata = gtk::Label::builder()
        .xalign(0.0)
        .yalign(0.0)
        .selectable(true)
        .wrap(true)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    let metadata_scroll = gtk::ScrolledWindow::builder()
        .width_request(340)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&metadata)
        .build();
    let metadata_panel = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideLeft)
        .transition_duration(180)
        .reveal_child(false)
        .child(&metadata_scroll)
        .build();

    let rotate_left = gtk::Button::with_label("↶ Rotate left");
    let rotate_right = gtk::Button::with_label("Rotate right ↷");
    let rotation_controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    rotation_controls.append(&rotate_left);
    rotation_controls.append(&rotate_right);
    let resize_edge = gtk::SpinButton::with_range(1.0, 100_000.0, 1.0);
    resize_edge.set_numeric(true);
    resize_edge.set_hexpand(true);
    let resize_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    resize_row.append(&gtk::Label::new(Some("Maximum edge")));
    resize_row.append(&resize_edge);
    resize_row.append(&gtk::Label::new(Some("px")));
    let edit_dimensions = gtk::Label::builder().xalign(0.0).build();
    edit_dimensions.add_css_class("dim-label");
    let reset_edits = gtk::Button::with_label("Reset");
    let save_copy = gtk::Button::with_label("Save Copy…");
    save_copy.add_css_class("suggested-action");
    let edit_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    edit_actions.set_halign(gtk::Align::End);
    edit_actions.append(&reset_edits);
    edit_actions.append(&save_copy);
    let edit_controls = gtk::Box::new(gtk::Orientation::Vertical, 12);
    edit_controls.set_width_request(340);
    edit_controls.set_margin_top(18);
    edit_controls.set_margin_bottom(18);
    edit_controls.set_margin_start(18);
    edit_controls.set_margin_end(18);
    edit_controls.append(
        &gtk::Label::builder()
            .label("Quick Edit")
            .xalign(0.0)
            .build(),
    );
    edit_controls.append(&rotation_controls);
    edit_controls.append(&resize_row);
    edit_controls.append(&edit_dimensions);
    edit_controls.append(&edit_actions);
    let edit_panel = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideLeft)
        .transition_duration(180)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Fill)
        .reveal_child(false)
        .child(&edit_controls)
        .build();
    edit_panel.add_css_class("edit-panel");

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&scroller));
    overlay.add_overlay(&status);
    overlay.add_overlay(&info);
    overlay.add_overlay(&edit_panel);
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    content.append(&overlay);
    content.append(&metadata_panel);

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .default_width(1200)
        .default_height(800)
        .child(&content)
        .build();

    let (loader_tx, loader_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    thread::Builder::new()
        .name("image-loader".into())
        .spawn(move || loader_loop(loader_rx, event_tx))
        .expect("could not start image loader");

    let cache_mb = env::var("BETTER_IMAGE_VIEW_CACHE_MB")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_CACHE_MB);

    let viewer = Rc::new(RefCell::new(Viewer {
        paths,
        index,
        direction: 1,
        generation: 1,
        canvas: canvas.clone(),
        scroller,
        status,
        info,
        metadata,
        metadata_panel,
        edit_panel,
        edit_rotation: Cell::new(0),
        resize_edge,
        edit_dimensions,
        window: window.clone(),
        cache: FrameCache::new(cache_mb * 1024 * 1024),
        loader_tx,
        skip_delete_confirmation: Cell::new(false),
    }));
    for property in ["width", "height"] {
        canvas.connect_notify_local(Some(property), {
            let viewer = viewer.clone();
            move |_, _| viewer.borrow().update_info()
        });
    }
    install_context_menu(viewer.clone());
    rotate_left.connect_clicked({
        let viewer = viewer.clone();
        move |_| viewer.borrow().rotate_edit(-1)
    });
    rotate_right.connect_clicked({
        let viewer = viewer.clone();
        move |_| viewer.borrow().rotate_edit(1)
    });
    reset_edits.connect_clicked({
        let viewer = viewer.clone();
        move |_| viewer.borrow().reset_edits()
    });
    viewer.borrow().resize_edge.connect_value_changed({
        let viewer = viewer.clone();
        move |_| {
            // set_value() emits this signal synchronously. Loading a frame also
            // sets the value while Viewer is mutably borrowed, so skip this
            // redundant callback in that case; accept() updates the label after.
            if let Ok(viewer) = viewer.try_borrow()
                && let Some(frame) = viewer.cache.frames.get(viewer.current_path())
            {
                viewer.update_edit_dimensions(frame);
            }
        }
    });
    save_copy.connect_clicked({
        let viewer = viewer.clone();
        move |_| request_save_copy(viewer.clone())
    });

    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed({
        let viewer = viewer.clone();
        let app = app.clone();
        move |_, key, _, state| {
            let control = state.contains(gdk::ModifierType::CONTROL_MASK);
            match key {
                gdk::Key::p | gdk::Key::P if control => viewer.borrow().print(),
                gdk::Key::Page_Down | gdk::Key::space => viewer.borrow_mut().move_by(1),
                gdk::Key::Page_Up | gdk::Key::BackSpace => viewer.borrow_mut().move_by(-1),
                gdk::Key::Right if viewer.borrow().canvas.is_zoomed() => {
                    viewer.borrow().pan_by(80.0, 0.0)
                }
                gdk::Key::Left if viewer.borrow().canvas.is_zoomed() => {
                    viewer.borrow().pan_by(-80.0, 0.0)
                }
                gdk::Key::Down if viewer.borrow().canvas.is_zoomed() => {
                    viewer.borrow().pan_by(0.0, 80.0)
                }
                gdk::Key::Up if viewer.borrow().canvas.is_zoomed() => {
                    viewer.borrow().pan_by(0.0, -80.0)
                }
                gdk::Key::Right => viewer.borrow_mut().move_by(1),
                gdk::Key::Left => viewer.borrow_mut().move_by(-1),
                gdk::Key::Home => viewer.borrow_mut().move_to(0),
                gdk::Key::End => {
                    let last = viewer.borrow().paths.len() - 1;
                    viewer.borrow_mut().move_to(last);
                }
                gdk::Key::plus | gdk::Key::KP_Add | gdk::Key::equal => viewer.borrow().zoom_in(),
                gdk::Key::minus | gdk::Key::KP_Subtract => viewer.borrow().zoom_out(),
                gdk::Key::_0 | gdk::Key::KP_0 => viewer.borrow().zoom_fit(),
                gdk::Key::f | gdk::Key::F => viewer.borrow().toggle_fullscreen(),
                gdk::Key::i | gdk::Key::I => viewer.borrow().toggle_info(),
                gdk::Key::e | gdk::Key::E => viewer.borrow().toggle_edit(),
                gdk::Key::p | gdk::Key::P => viewer.borrow().toggle_metadata(),
                gdk::Key::Delete => request_delete(viewer.clone()),
                gdk::Key::Escape | gdk::Key::q => app.quit(),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        }
    });
    window.add_controller(keys);

    let scroll_accumulator = Rc::new(Cell::new(0.0_f64));
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    scroll.connect_scroll({
        let viewer = viewer.clone();
        let accumulator = scroll_accumulator.clone();
        move |_, _, dy| {
            let total = accumulator.get() + dy;
            if total.abs() >= 1.0 {
                viewer
                    .borrow_mut()
                    .move_by(if total > 0.0 { 1 } else { -1 });
                accumulator.set(0.0);
            } else {
                accumulator.set(total);
            }
            glib::Propagation::Stop
        }
    });
    window.add_controller(scroll);

    glib::timeout_add_local(Duration::from_millis(16), {
        let viewer = viewer.clone();
        move || {
            while let Ok(event) = event_rx.try_recv() {
                viewer.borrow_mut().accept(event);
            }
            glib::ControlFlow::Continue
        }
    });

    window.connect_close_request({
        let viewer = viewer.clone();
        move |_| {
            let _ = viewer.borrow().loader_tx.send(LoaderCommand::Stop);
            glib::Propagation::Proceed
        }
    });

    viewer.borrow_mut().show_or_load();
    window.fullscreen();
    window.present();

    // Debug aid: BIV_DEBUG_PRINT=1 opens the print dialog shortly after startup.
    if env::var("BIV_DEBUG_PRINT").is_ok() {
        let viewer = viewer.clone();
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            viewer.borrow().print();
        });
    }
}

fn install_context_menu(viewer: Rc<RefCell<Viewer>>) {
    let canvas = viewer.borrow().canvas.clone();
    let open_with = gtk::Button::with_label("Open With…");
    let copy_bitmap = gtk::Button::with_label("Copy Bitmap");
    let copy_filename = gtk::Button::with_label("Copy Filename");
    for button in [&open_with, &copy_bitmap, &copy_filename] {
        button.set_halign(gtk::Align::Fill);
        button.set_hexpand(true);
        button.add_css_class("flat");
        if let Some(label) = button.child().and_downcast::<gtk::Label>() {
            label.set_xalign(0.0);
        }
    }
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
    menu.set_margin_top(6);
    menu.set_margin_bottom(6);
    menu.set_margin_start(6);
    menu.set_margin_end(6);
    menu.append(&open_with);
    menu.append(&copy_bitmap);
    menu.append(&copy_filename);
    let popover = gtk::Popover::builder()
        .autohide(true)
        .has_arrow(false)
        .child(&menu)
        .build();
    popover.set_parent(&canvas);

    open_with.connect_clicked({
        let viewer = viewer.clone();
        let popover = popover.clone();
        move |_| {
            popover.popdown();
            viewer.borrow().open_with();
        }
    });
    copy_bitmap.connect_clicked({
        let viewer = viewer.clone();
        let popover = popover.clone();
        move |_| {
            viewer.borrow().copy_bitmap();
            popover.popdown();
        }
    });
    copy_filename.connect_clicked({
        let viewer = viewer.clone();
        let popover = popover.clone();
        move |_| {
            viewer.borrow().copy_filename();
            popover.popdown();
        }
    });

    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_SECONDARY);
    click.connect_pressed(move |_, _, x, y| {
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.popup();
    });
    canvas.add_controller(click);
}

fn request_save_copy(viewer: Rc<RefCell<Viewer>>) {
    let (source, rotation, maximum_edge, window) = {
        let viewer = viewer.borrow();
        (
            viewer.current_path().to_path_buf(),
            viewer.edit_rotation.get(),
            viewer.resize_edge.value() as i32,
            viewer.window.clone(),
        )
    };
    let stem = source
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("image");
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("jpg");
    let dialog = gtk::FileDialog::builder()
        .title("Save Edited Copy")
        .initial_name(format!("{stem}-edited.{extension}"))
        .build();
    if let Some(parent) = source.parent() {
        dialog.set_initial_folder(Some(&gtk::gio::File::for_path(parent)));
    }
    dialog.save(Some(&window), gtk::gio::Cancellable::NONE, move |result| {
        let Ok(file) = result else {
            return;
        };
        let Some(destination) = file.path() else {
            viewer
                .borrow()
                .status
                .set_text("Choose a local destination");
            return;
        };
        viewer.borrow().status.set_text("Saving edited copy…");
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("image-export".into())
            .spawn(move || {
                let result = save_edited_copy(&source, &destination, rotation, maximum_edge);
                let _ = tx.send(result.map(|()| destination));
            })
            .expect("could not start export worker");
        glib::timeout_add_local(Duration::from_millis(30), move || match rx.try_recv() {
            Ok(Ok(path)) => {
                viewer
                    .borrow()
                    .status
                    .set_text(&format!("Saved {}", path.display()));
                glib::ControlFlow::Break
            }
            Ok(Err(message)) => {
                viewer
                    .borrow()
                    .status
                    .set_text(&format!("Could not save: {message}"));
                glib::ControlFlow::Break
            }
            Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => {
                viewer.borrow().status.set_text("Export worker stopped");
                glib::ControlFlow::Break
            }
        });
    });
}

fn save_edited_copy(
    source_path: &Path,
    destination: &Path,
    rotation: u8,
    maximum_edge: i32,
) -> Result<(), String> {
    let source_name = source_path
        .to_str()
        .ok_or_else(|| "source filename is not valid UTF-8".to_string())?;
    let destination_name = destination
        .to_str()
        .ok_or_else(|| "destination filename is not valid UTF-8".to_string())?;
    let image = VipsImage::new_from_file(source_name).map_err(|error| error.to_string())?;
    // Some loaders reject malformed orientation metadata. Treat that as
    // "already upright" rather than making an otherwise valid export fail.
    let image = ops::autorot(&image).unwrap_or(image);
    let image = match rotation % 4 {
        0 => image,
        1 => ops::rot(&image, ops::Angle::D90).map_err(|error| error.to_string())?,
        2 => ops::rot(&image, ops::Angle::D180).map_err(|error| error.to_string())?,
        _ => ops::rot(&image, ops::Angle::D270).map_err(|error| error.to_string())?,
    };
    let edge = image.get_width().max(image.get_height());
    let image = if maximum_edge > 0 && maximum_edge < edge {
        ops::resize(&image, maximum_edge as f64 / edge as f64).map_err(|error| error.to_string())?
    } else {
        image
    };
    image
        .image_write_to_file(destination_name)
        .map_err(|error| error.to_string())
}

#[cfg(any())]
fn show_print_setup(
    parent: &gtk::ApplicationWindow,
    status: &gtk::Label,
    frame: Arc<DecodedFrame>,
) {
    let window = gtk::Window::builder()
        .title("Image Print Setup")
        .transient_for(parent)
        .modal(true)
        .default_width(760)
        .default_height(580)
        .build();
    let paper = gtk::Fixed::new();
    paper.set_overflow(gtk::Overflow::Hidden);
    paper.add_css_class("print-paper");
    let bytes = glib::Bytes::from_owned(frame.pixels.clone());
    let texture = gdk::MemoryTexture::new(
        frame.width,
        frame.height,
        gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        frame.stride,
    );
    let preview_image = gtk::Picture::builder()
        .paintable(&texture)
        .content_fit(gtk::ContentFit::Fill)
        .can_shrink(true)
        .build();
    paper.put(&preview_image, 0.0, 0.0);

    let paper_choice = gtk::DropDown::from_strings(&["A4", "US Letter"]);
    let orientation = gtk::DropDown::from_strings(&["Portrait", "Landscape"]);
    let sizing = gtk::DropDown::from_strings(&["Fit to printable area", "Image resolution (DPI)"]);
    let native_dpi = frame.source_dpi.unwrap_or(300.0);
    let dpi = gtk::SpinButton::with_range(36.0, 2400.0, 1.0);
    dpi.set_value(native_dpi);
    dpi.set_sensitive(false);
    let dpi_note = gtk::Label::builder()
        .label(if frame.source_dpi.is_some() {
            format!("Embedded resolution: {native_dpi:.0} DPI")
        } else {
            "No trustworthy embedded resolution; using 300 DPI".to_string()
        })
        .xalign(0.0)
        .wrap(true)
        .build();
    dpi_note.add_css_class("dim-label");

    let controls = gtk::Grid::builder()
        .row_spacing(12)
        .column_spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    controls.attach(&gtk::Label::new(Some("Paper")), 0, 0, 1, 1);
    controls.attach(&paper_choice, 1, 0, 1, 1);
    controls.attach(&gtk::Label::new(Some("Orientation")), 0, 1, 1, 1);
    controls.attach(&orientation, 1, 1, 1, 1);
    controls.attach(&gtk::Label::new(Some("Sizing")), 0, 2, 1, 1);
    controls.attach(&sizing, 1, 2, 1, 1);
    controls.attach(&gtk::Label::new(Some("Resolution")), 0, 3, 1, 1);
    controls.attach(&dpi, 1, 3, 1, 1);
    controls.attach(&dpi_note, 0, 4, 2, 1);

    let cancel = gtk::Button::with_label("Cancel (Esc)");
    let print = gtk::Button::with_label("Continue to Printer…");
    print.add_css_class("suggested-action");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.append(&cancel);
    buttons.append(&print);
    controls.attach(&buttons, 0, 5, 2, 1);

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.append(&paper);
    content.append(&controls);
    window.set_child(Some(&content));
    window.set_default_widget(Some(&print));

    let update_preview: Rc<dyn Fn()> = Rc::new({
        let paper = paper.clone();
        let preview_image = preview_image.clone();
        let paper_choice = paper_choice.clone();
        let orientation = orientation.clone();
        let sizing = sizing.clone();
        let dpi = dpi.clone();
        let frame = frame.clone();
        move || {
            let spec = print_spec(&paper_choice, &orientation, &sizing, &dpi);
            dpi.set_sensitive(!spec.fit_to_page);
            let ratio = spec.paper_width_mm / spec.paper_height_mm;
            let (paper_width, paper_height) = if ratio > 1.0 {
                (460, (460.0 / ratio) as i32)
            } else {
                ((480.0 * ratio) as i32, 480)
            };
            paper.set_size_request(paper_width, paper_height);
            let (x, y, width, height) =
                spec.image_rect_points(frame.source_width, frame.source_height);
            let points_width = spec.paper_width_mm / 25.4 * 72.0;
            let points_height = spec.paper_height_mm / 25.4 * 72.0;
            preview_image.set_size_request(
                (width / points_width * paper_width as f64).round() as i32,
                (height / points_height * paper_height as f64).round() as i32,
            );
            paper.move_(
                &preview_image,
                x / points_width * paper_width as f64,
                y / points_height * paper_height as f64,
            );
        }
    });
    for choice in [&paper_choice, &orientation, &sizing] {
        choice.connect_selected_notify({
            let update_preview = update_preview.clone();
            move |_| update_preview()
        });
    }
    dpi.connect_value_changed({
        let update_preview = update_preview.clone();
        move |_| update_preview()
    });
    update_preview();

    cancel.connect_clicked({
        let window = window.clone();
        move |_| window.close()
    });
    print.connect_clicked({
        let window = window.clone();
        let parent = parent.clone();
        let status = status.clone();
        move |_| {
            let spec = print_spec(&paper_choice, &orientation, &sizing, &dpi);
            window.close();
            prepare_print(parent.clone(), status.clone(), frame.clone(), spec);
        }
    });
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed({
        let window = window.clone();
        move |_, key, _, _| {
            if key == gdk::Key::Escape {
                window.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    });
    window.add_controller(keys);
    window.present();
    print.grab_focus();
}

#[cfg(any())]
fn print_spec(
    paper: &gtk::DropDown,
    orientation: &gtk::DropDown,
    sizing: &gtk::DropDown,
    dpi: &gtk::SpinButton,
) -> PrintSpec {
    let (mut width, mut height) = if paper.selected() == 1 {
        (215.9, 279.4)
    } else {
        (210.0, 297.0)
    };
    if orientation.selected() == 1 {
        std::mem::swap(&mut width, &mut height);
    }
    PrintSpec {
        paper_width_mm: width,
        paper_height_mm: height,
        fit_to_page: sizing.selected() == 0,
        image_dpi: dpi.value(),
    }
}

#[cfg(any())]
fn prepare_print(
    parent: gtk::ApplicationWindow,
    status: gtk::Label,
    frame: Arc<DecodedFrame>,
    spec: PrintSpec,
) {
    status.set_text("Preparing print document…");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(create_print_pdf(&frame, spec));
    });
    glib::timeout_add_local(Duration::from_millis(50), move || match rx.try_recv() {
        Ok(Ok(path)) => {
            status.set_text("");
            let file = gtk::gio::File::for_path(&path);
            let dialog = gtk::PrintDialog::new();
            dialog.set_modal(true);
            dialog.set_title("Print Image");
            let status = status.clone();
            dialog.print_file(
                Some(&parent),
                None,
                &file,
                gtk::gio::Cancellable::NONE,
                move |result| {
                    let _ = std::fs::remove_file(&path);
                    if let Err(error) = result {
                        if !error.matches(gtk::gio::IOErrorEnum::Cancelled) {
                            status.set_text(&format!("Could not print: {error}"));
                        }
                    }
                },
            );
            glib::ControlFlow::Break
        }
        Ok(Err(error)) => {
            status.set_text(&format!("Could not prepare print: {error}"));
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => {
            status.set_text("Print preparation stopped unexpectedly");
            glib::ControlFlow::Break
        }
    });
}

#[cfg(any())]
fn create_print_pdf(frame: &DecodedFrame, spec: PrintSpec) -> Result<PathBuf, String> {
    let paper_width = spec.paper_width_mm / 25.4 * 72.0;
    let paper_height = spec.paper_height_mm / 25.4 * 72.0;
    let (_, _, draw_width, draw_height) =
        spec.image_rect_points(frame.source_width, frame.source_height);
    let target_width = ((draw_width / 72.0 * PRINT_RASTER_DPI).ceil() as i32)
        .min(frame.source_width)
        .max(1);
    let options = ops::ThumbnailOptions {
        height: ((draw_height / 72.0 * PRINT_RASTER_DPI).ceil() as i32)
            .min(frame.source_height)
            .max(1),
        size: ops::Size::Down,
        ..Default::default()
    };
    let source = frame
        .path
        .to_str()
        .ok_or_else(|| "filename is not valid UTF-8".to_string())?;
    let image =
        ops::thumbnail_with_opts(source, target_width, &options).map_err(|e| e.to_string())?;
    let png = ops::pngsave_buffer(&image).map_err(|e| e.to_string())?;
    let mut cursor = Cursor::new(png);
    let image = cairo::ImageSurface::create_from_png(&mut cursor).map_err(|e| e.to_string())?;
    let path = env::temp_dir().join(format!(
        "better-image-view-print-{}-{}.pdf",
        std::process::id(),
        glib::monotonic_time()
    ));
    let surface =
        cairo::PdfSurface::new(paper_width, paper_height, &path).map_err(|e| e.to_string())?;
    let context = cairo::Context::new(&surface).map_err(|e| e.to_string())?;
    context.set_source_rgb(1.0, 1.0, 1.0);
    context.paint().map_err(|e| e.to_string())?;
    let (x, y, width, height) = spec.image_rect_points(frame.source_width, frame.source_height);
    context.save().map_err(|e| e.to_string())?;
    context.rectangle(0.0, 0.0, paper_width, paper_height);
    context.clip();
    context.translate(x, y);
    context.scale(width / image.width() as f64, height / image.height() as f64);
    context
        .set_source_surface(&image, 0.0, 0.0)
        .map_err(|e| e.to_string())?;
    context.source().set_filter(cairo::Filter::Best);
    context.paint().map_err(|e| e.to_string())?;
    context.restore().map_err(|e| e.to_string())?;
    context.show_page().map_err(|e| e.to_string())?;
    surface.finish();
    surface.status().map_err(|e| e.to_string())?;
    Ok(path)
}

struct PrintUi {
    spec: Rc<Cell<PrintSpec>>,
    preview: gtk::DrawingArea,
    frame: Arc<DecodedFrame>,
    root: gtk::Widget,
}

impl PrintUi {
    fn update_preview(&self) {
        self.preview.queue_draw();
    }
}

fn run_print_dialog(
    parent: &gtk::ApplicationWindow,
    status: &gtk::Label,
    frame: Arc<DecodedFrame>,
) {
    // GtkPrintOperation routes through the print portal on current GTK, and the
    // portal dialog cannot embed custom tabs. Drive the in-process print dialog
    // directly and submit the rendered page as a print job, so the Image
    // Settings tab with its live preview lives inside the dialog like GIMP's.
    #[allow(deprecated)]
    let dialog = gtk::PrintUnixDialog::new(Some("Print Image"), Some(parent));
    dialog.set_modal(true);
    dialog.set_embed_page_setup(true);
    let (widget, ui) = build_print_tab(&dialog.page_setup(), frame.clone());
    dialog.add_custom_tab(&widget, &gtk::Label::new(Some("Image Settings")));

    fn sync_page_setup(dialog: &gtk::PrintUnixDialog, ui: &PrintUi) {
        let setup = dialog.page_setup();
        let mut spec = ui.spec.get();
        spec.paper_width_mm = setup.paper_width(gtk::Unit::Mm);
        spec.paper_height_mm = setup.paper_height(gtk::Unit::Mm);
        ui.spec.set(spec);
        ui.update_preview();
    }
    dialog.connect_page_setup_notify({
        let ui = ui.clone();
        move |dialog| sync_page_setup(dialog, &ui)
    });
    dialog.connect_selected_printer_notify({
        let ui = ui.clone();
        move |dialog| sync_page_setup(dialog, &ui)
    });

    #[allow(deprecated)]
    dialog.connect_response({
        let status = status.clone();
        let ui = ui.clone();
        let frame = frame.clone();
        move |dialog, response| {
            if response == gtk::ResponseType::Ok {
                let Some(printer) = dialog.selected_printer() else {
                    status.set_text("No printer selected");
                    dialog.close();
                    return;
                };
                let settings = dialog.settings();
                let setup = dialog.page_setup();
                let title = frame
                    .path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("Image");
                let resolution = settings.resolution();
                let raster_dpi = if resolution > 0 {
                    resolution as f64
                } else {
                    300.0
                };
                let job = gtk::PrintJob::new(title, &printer, &settings, &setup);
                match render_print_job(&job, &frame, ui.spec.get(), raster_dpi) {
                    Ok(()) => {
                        let status = status.clone();
                        job.send(move |job, result| match result {
                            Ok(()) => status.set_text(&format!(
                                "Sent “{}” to {}",
                                job.title(),
                                job.printer().name()
                            )),
                            Err(error) => status.set_text(&format!("Could not print: {error}")),
                        });
                    }
                    Err(error) => status.set_text(&format!("Could not render print: {error}")),
                }
            }
            dialog.close();
        }
    });

    // Debug aid: jump straight to the custom tab so it can be inspected
    // without clicking through the dialog.
    if env::var("BIV_DEBUG_PRINT").is_ok() {
        let ui = ui.clone();
        glib::timeout_add_local(Duration::from_millis(1200), move || {
            let Some(notebook) = ui
                .root
                .ancestor(gtk::Notebook::static_type())
                .and_downcast::<gtk::Notebook>()
            else {
                return glib::ControlFlow::Break;
            };
            for index in 0..notebook.n_pages() {
                let Some(page) = notebook.nth_page(Some(index)) else {
                    continue;
                };
                if page == *ui.root.upcast_ref::<gtk::Widget>() || ui.root.is_ancestor(&page) {
                    notebook.set_current_page(Some(index));
                    break;
                }
            }
            glib::ControlFlow::Break
        });
    }
    dialog.present();
}

fn build_print_tab(setup: &gtk::PageSetup, frame: Arc<DecodedFrame>) -> (gtk::Widget, Rc<PrintUi>) {
    let dpi_value = frame.source_dpi.unwrap_or(300.0);
    let spec = Rc::new(Cell::new(PrintSpec {
        paper_width_mm: setup.paper_width(gtk::Unit::Mm),
        paper_height_mm: setup.paper_height(gtk::Unit::Mm),
        fit_to_page: false,
        image_dpi: dpi_value,
        align_x: 0.5,
        align_y: 0.5,
        offset_x_mm: 0.0,
        offset_y_mm: 0.0,
    }));
    let preview = gtk::DrawingArea::new();
    preview.set_content_width(420);
    preview.set_content_height(360);
    preview.set_hexpand(true);
    preview.set_vexpand(true);
    match frame_preview_surface(&frame) {
        Ok(image) => {
            let spec = spec.clone();
            let (source_width, source_height) = (frame.source_width, frame.source_height);
            preview.set_draw_func(move |_, cr, width, height| {
                draw_paper_preview(
                    cr,
                    width,
                    height,
                    spec.get(),
                    &image,
                    source_width,
                    source_height,
                );
            });
        }
        Err(error) => eprintln!("print preview unavailable: {error}"),
    }
    let root = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    let ui = PrintUi {
        spec,
        preview: preview.clone(),
        frame,
        root: root.clone().upcast(),
    };

    let sizing = gtk::DropDown::from_strings(&["Image resolution (DPI)", "Fit to printable area"]);
    let dpi = gtk::SpinButton::with_range(36.0, 2400.0, 1.0);
    dpi.set_value(dpi_value);
    let note = gtk::Label::builder()
        .label(if ui.frame.source_dpi.is_some() {
            format!("Using embedded resolution: {dpi_value:.0} DPI")
        } else {
            "No trustworthy embedded resolution; using 300 DPI".to_string()
        })
        .xalign(0.0)
        .wrap(true)
        .build();
    note.add_css_class("dim-label");
    let align_x = gtk::DropDown::from_strings(&["Left", "Center", "Right"]);
    align_x.set_selected(1);
    let align_y = gtk::DropDown::from_strings(&["Top", "Middle", "Bottom"]);
    align_y.set_selected(1);
    let offset_x = gtk::SpinButton::with_range(-1000.0, 1000.0, 1.0);
    offset_x.set_digits(1);
    offset_x.set_value(0.0);
    let offset_y = gtk::SpinButton::with_range(-1000.0, 1000.0, 1.0);
    offset_y.set_digits(1);
    offset_y.set_value(0.0);
    let drag_note = gtk::Label::builder()
        .label("Drag the image in the preview\nto adjust the offset.")
        .xalign(0.0)
        .build();
    drag_note.add_css_class("dim-label");
    let controls = gtk::Grid::builder()
        .row_spacing(12)
        .column_spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    controls.attach(&gtk::Label::new(Some("Sizing")), 0, 0, 1, 1);
    controls.attach(&sizing, 1, 0, 1, 1);
    controls.attach(&gtk::Label::new(Some("Resolution")), 0, 1, 1, 1);
    controls.attach(&dpi, 1, 1, 1, 1);
    controls.attach(&note, 0, 2, 2, 1);
    controls.attach(&gtk::Label::new(Some("Horizontal")), 0, 3, 1, 1);
    controls.attach(&align_x, 1, 3, 1, 1);
    controls.attach(&gtk::Label::new(Some("Vertical")), 0, 4, 1, 1);
    controls.attach(&align_y, 1, 4, 1, 1);
    controls.attach(&gtk::Label::new(Some("Offset X (mm)")), 0, 5, 1, 1);
    controls.attach(&offset_x, 1, 5, 1, 1);
    controls.attach(&gtk::Label::new(Some("Offset Y (mm)")), 0, 6, 1, 1);
    controls.attach(&offset_y, 1, 6, 1, 1);
    controls.attach(&drag_note, 0, 7, 2, 1);
    root.set_margin_top(12);
    root.set_margin_bottom(12);
    root.set_margin_start(12);
    root.set_margin_end(12);
    root.append(&preview);
    root.append(&controls);

    let shared = Rc::new(ui);
    sizing.connect_selected_notify({
        let shared = shared.clone();
        let dpi = dpi.clone();
        move |sizing| {
            let mut spec = shared.spec.get();
            spec.fit_to_page = sizing.selected() == 1;
            shared.spec.set(spec);
            dpi.set_sensitive(!spec.fit_to_page);
            shared.update_preview();
        }
    });
    dpi.connect_value_changed({
        let shared = shared.clone();
        move |dpi| {
            let mut spec = shared.spec.get();
            spec.image_dpi = dpi.value();
            shared.spec.set(spec);
            shared.update_preview();
        }
    });
    align_x.connect_selected_notify({
        let shared = shared.clone();
        move |align_x| {
            let mut spec = shared.spec.get();
            spec.align_x = align_x.selected() as f64 / 2.0;
            shared.spec.set(spec);
            shared.update_preview();
        }
    });
    align_y.connect_selected_notify({
        let shared = shared.clone();
        move |align_y| {
            let mut spec = shared.spec.get();
            spec.align_y = align_y.selected() as f64 / 2.0;
            shared.spec.set(spec);
            shared.update_preview();
        }
    });
    offset_x.connect_value_changed({
        let shared = shared.clone();
        move |offset_x| {
            let mut spec = shared.spec.get();
            spec.offset_x_mm = offset_x.value();
            shared.spec.set(spec);
            shared.update_preview();
        }
    });
    offset_y.connect_value_changed({
        let shared = shared.clone();
        move |offset_y| {
            let mut spec = shared.spec.get();
            spec.offset_y_mm = offset_y.value();
            shared.spec.set(spec);
            shared.update_preview();
        }
    });

    // Dragging the image on the preview adjusts the offsets; the spin buttons
    // stay in sync because their value-changed handlers write the spec back.
    let drag = gtk::GestureDrag::new();
    let drag_start: Rc<Cell<(f64, f64)>> = Rc::new(Cell::new((0.0, 0.0)));
    drag.connect_drag_begin({
        let shared = shared.clone();
        let drag_start = drag_start.clone();
        move |_, _, _| {
            let spec = shared.spec.get();
            drag_start.set((spec.offset_x_mm, spec.offset_y_mm));
        }
    });
    drag.connect_drag_update({
        let shared = shared.clone();
        let offset_x = offset_x.clone();
        let offset_y = offset_y.clone();
        move |_, dx, dy| {
            let spec = shared.spec.get();
            let Some((scale, ..)) =
                preview_sheet_layout(shared.preview.width(), shared.preview.height(), spec)
            else {
                return;
            };
            let points_per_mm = 72.0 / 25.4;
            let (start_x, start_y) = drag_start.get();
            offset_x.set_value(start_x + dx / scale / points_per_mm);
            offset_y.set_value(start_y + dy / scale / points_per_mm);
        }
    });
    preview.add_controller(drag);
    shared.update_preview();
    (root.upcast(), shared)
}

/// Render one page into the job's spool surface. The geometry comes from the
/// same PrintSpec the preview draws, so what prints is what the tab shows.
fn render_print_job(
    job: &gtk::PrintJob,
    frame: &DecodedFrame,
    spec: PrintSpec,
    raster_dpi: f64,
) -> Result<(), String> {
    let surface = job.surface().map_err(|e| e.to_string())?;
    let cairo = cairo::Context::new(&surface).map_err(|e| e.to_string())?;
    let paper_width = spec.paper_width_mm / 25.4 * 72.0;
    let paper_height = spec.paper_height_mm / 25.4 * 72.0;
    let (x, y, draw_width, draw_height) =
        spec.image_rect_points(frame.source_width, frame.source_height);
    let target_width = ((draw_width / 72.0 * raster_dpi).ceil() as i32)
        .min(frame.source_width)
        .max(1);
    let target_height = ((draw_height / 72.0 * raster_dpi).ceil() as i32)
        .min(frame.source_height)
        .max(1);
    let source = frame
        .path
        .to_str()
        .ok_or_else(|| "filename is not valid UTF-8".to_string())?;
    let image = ops::thumbnail_with_opts(
        source,
        target_width,
        &ops::ThumbnailOptions {
            height: target_height,
            size: ops::Size::Down,
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())?;
    let png = ops::pngsave_buffer(&image).map_err(|e| e.to_string())?;
    let image =
        cairo::ImageSurface::create_from_png(&mut Cursor::new(png)).map_err(|e| e.to_string())?;
    cairo.save().map_err(|e| e.to_string())?;
    cairo.rectangle(0.0, 0.0, paper_width, paper_height);
    cairo.clip();
    cairo.translate(x, y);
    cairo.scale(
        draw_width / image.width() as f64,
        draw_height / image.height() as f64,
    );
    cairo
        .set_source_surface(&image, 0.0, 0.0)
        .map_err(|e| e.to_string())?;
    cairo.source().set_filter(cairo::Filter::Best);
    cairo.paint().map_err(|e| e.to_string())?;
    cairo.restore().map_err(|e| e.to_string())?;
    cairo.show_page().map_err(|e| e.to_string())?;
    drop(cairo);
    surface.finish();
    surface.status().map_err(|e| e.to_string())?;
    Ok(())
}

/// Convert the decoded RGBA frame into a premultiplied ARGB32 cairo surface
/// for the in-dialog preview.
fn frame_preview_surface(frame: &DecodedFrame) -> Result<cairo::ImageSurface, String> {
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, frame.width, frame.height)
        .map_err(|e| e.to_string())?;
    let dst_stride = surface.stride() as usize;
    {
        let mut data = surface.data().map_err(|e| e.to_string())?;
        for row in 0..frame.height as usize {
            let src = &frame.pixels[row * frame.stride..];
            let dst = &mut data[row * dst_stride..];
            for column in 0..frame.width as usize {
                let (r, g, b, a) = (
                    src[column * 4],
                    src[column * 4 + 1],
                    src[column * 4 + 2],
                    src[column * 4 + 3],
                );
                let premultiply = |channel: u8| ((channel as u16 * a as u16) / 255) as u8;
                dst[column * 4] = premultiply(b);
                dst[column * 4 + 1] = premultiply(g);
                dst[column * 4 + 2] = premultiply(r);
                dst[column * 4 + 3] = a;
            }
        }
    }
    Ok(surface)
}

/// Draw the paper sheet centered in the widget with the image placed on it
/// according to the current PrintSpec.
/// Fit the paper sheet into the preview widget. Returns
/// (pixels per point, sheet x, sheet y, sheet width, sheet height).
fn preview_sheet_layout(
    width: i32,
    height: i32,
    spec: PrintSpec,
) -> Option<(f64, f64, f64, f64, f64)> {
    let paper_width = spec.paper_width_mm / 25.4 * 72.0;
    let paper_height = spec.paper_height_mm / 25.4 * 72.0;
    let padding = 12.0;
    let scale = ((width as f64 - padding * 2.0) / paper_width)
        .min((height as f64 - padding * 2.0) / paper_height);
    if scale <= 0.0 || !scale.is_finite() {
        return None;
    }
    let sheet_width = paper_width * scale;
    let sheet_height = paper_height * scale;
    Some((
        scale,
        (width as f64 - sheet_width) / 2.0,
        (height as f64 - sheet_height) / 2.0,
        sheet_width,
        sheet_height,
    ))
}

fn draw_paper_preview(
    cr: &cairo::Context,
    width: i32,
    height: i32,
    spec: PrintSpec,
    image: &cairo::ImageSurface,
    source_width: i32,
    source_height: i32,
) {
    let Some((scale, sheet_x, sheet_y, sheet_width, sheet_height)) =
        preview_sheet_layout(width, height, spec)
    else {
        return;
    };
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.3);
    cr.rectangle(sheet_x + 3.0, sheet_y + 3.0, sheet_width, sheet_height);
    let _ = cr.fill();
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.rectangle(sheet_x, sheet_y, sheet_width, sheet_height);
    let _ = cr.fill_preserve();
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.5);
    cr.set_line_width(1.0);
    let _ = cr.stroke();
    let (x, y, draw_width, draw_height) = spec.image_rect_points(source_width, source_height);
    let _ = cr.save();
    cr.rectangle(sheet_x, sheet_y, sheet_width, sheet_height);
    cr.clip();
    cr.translate(sheet_x + x * scale, sheet_y + y * scale);
    cr.scale(
        draw_width * scale / image.width() as f64,
        draw_height * scale / image.height() as f64,
    );
    if cr.set_source_surface(image, 0.0, 0.0).is_ok() {
        cr.source().set_filter(cairo::Filter::Good);
        let _ = cr.paint();
    }
    let _ = cr.restore();
}

#[derive(Clone, Copy)]
enum DeleteMode {
    Trash,
    Permanent,
}

fn delete_mode(path: &Path) -> DeleteMode {
    let file = gtk::gio::File::for_path(path);
    let can_trash = file
        .query_info(
            gtk::gio::FILE_ATTRIBUTE_ACCESS_CAN_TRASH,
            gtk::gio::FileQueryInfoFlags::NONE,
            gtk::gio::Cancellable::NONE,
        )
        .map(|info| info.boolean(gtk::gio::FILE_ATTRIBUTE_ACCESS_CAN_TRASH))
        .unwrap_or(false);
    if can_trash {
        DeleteMode::Trash
    } else {
        DeleteMode::Permanent
    }
}

fn request_delete(viewer: Rc<RefCell<Viewer>>) {
    let (path, window) = {
        let viewer = viewer.borrow();
        (viewer.current_path().to_path_buf(), viewer.window.clone())
    };
    let mode = delete_mode(&path);
    if viewer.borrow().skip_delete_confirmation.get() {
        perform_delete(viewer, path, mode);
        return;
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("this image");
    let (message, detail, action) = match mode {
        DeleteMode::Trash => (
            format!("Move “{name}” to Trash?"),
            "You can restore it from the desktop Trash.",
            "Move to Trash",
        ),
        DeleteMode::Permanent => (
            format!("Permanently delete “{name}”?"),
            "Trash is unavailable for this location. This cannot be undone.",
            "Delete Permanently",
        ),
    };

    let prompt = gtk::Window::builder()
        .title("Confirm deletion")
        .transient_for(&window)
        .modal(true)
        .resizable(false)
        .build();
    let heading = gtk::Label::builder()
        .label(message)
        .xalign(0.0)
        .wrap(true)
        .build();
    heading.add_css_class("title-3");
    let explanation = gtk::Label::builder()
        .label(detail)
        .xalign(0.0)
        .wrap(true)
        .build();
    let remember = gtk::CheckButton::with_label("Remember decision for this session");
    let cancel = gtk::Button::with_label("Cancel (Esc)");
    let confirm = gtk::Button::with_label(action);
    confirm.add_css_class("destructive-action");
    confirm.add_css_class("delete-confirm");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::Start);
    buttons.append(&confirm);
    buttons.append(&cancel);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);
    content.set_margin_end(24);
    content.set_width_request(420);
    content.append(&heading);
    content.append(&explanation);
    content.append(&buttons);
    content.append(&remember);
    prompt.set_child(Some(&content));
    prompt.set_default_widget(Some(&confirm));

    cancel.connect_clicked({
        let prompt = prompt.clone();
        move |_| prompt.close()
    });
    confirm.connect_clicked({
        let prompt = prompt.clone();
        let remember_choice = remember.clone();
        move |_| {
            if remember_choice.is_active() {
                viewer.borrow().skip_delete_confirmation.set(true);
            }
            prompt.close();
            perform_delete(viewer.clone(), path.clone(), mode);
        }
    });
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed({
        let prompt = prompt.clone();
        let remember = remember.clone();
        let confirm = confirm.clone();
        move |_, key, _, _| {
            if key == gdk::Key::Escape {
                prompt.close();
                glib::Propagation::Stop
            } else if key == gdk::Key::Up && remember.has_focus() {
                confirm.grab_focus();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    });
    prompt.add_controller(keys);
    prompt.present();
    confirm.grab_focus();
}

fn perform_delete(viewer: Rc<RefCell<Viewer>>, path: PathBuf, mode: DeleteMode) {
    let result = match mode {
        DeleteMode::Trash => gtk::gio::File::for_path(&path)
            .trash(gtk::gio::Cancellable::NONE)
            .map_err(|error| format!("Could not move to Trash: {error}")),
        DeleteMode::Permanent => {
            std::fs::remove_file(&path).map_err(|error| format!("Could not delete: {error}"))
        }
    };
    match result {
        Ok(()) => viewer.borrow_mut().forget_current_image(),
        Err(message) => viewer.borrow().status.set_text(&message),
    }
}

fn loader_loop(rx: mpsc::Receiver<LoaderCommand>, tx: mpsc::Sender<LoaderEvent>) {
    let _vips = match VipsApp::new("better-image-view", false) {
        Ok(app) => app,
        Err(error) => {
            eprintln!("could not initialize libvips: {error}");
            return;
        }
    };

    let mut pending = None;
    loop {
        let command = match pending.take() {
            Some(command) => command,
            None => match rx.recv() {
                Ok(command) => command,
                Err(_) => return,
            },
        };
        let LoaderCommand::Batch {
            mut generation,
            mut paths,
        } = command
        else {
            break;
        };

        loop {
            match rx.try_recv() {
                Ok(LoaderCommand::Batch {
                    generation: newer_generation,
                    paths: newer_paths,
                }) => {
                    generation = newer_generation;
                    paths = newer_paths;
                }
                Ok(LoaderCommand::Stop) => return,
                Err(_) => break,
            }
        }

        for path in paths {
            if let Ok(newer) = rx.try_recv() {
                match newer {
                    LoaderCommand::Batch { .. } => {
                        pending = Some(newer);
                        break;
                    }
                    LoaderCommand::Stop => return,
                }
            }
            match decode_thumbnail(&path) {
                Ok(frame) => {
                    if tx.send(LoaderEvent::Loaded { generation, frame }).is_err() {
                        return;
                    }
                }
                Err(message) => {
                    let _ = tx.send(LoaderEvent::Failed {
                        generation,
                        path,
                        message,
                    });
                }
            }
        }
    }
}

fn decode_thumbnail(path: &Path) -> Result<DecodedFrame, String> {
    let filename = path
        .to_str()
        .ok_or_else(|| "filename is not valid UTF-8".to_string())?;
    let raw_source = VipsImage::new_from_file(filename).map_err(|e| e.to_string())?;
    // vips_thumbnail() below already rotates from EXIF orientation. Avoid an
    // additional autorot operation in the hot loading path: malformed metadata
    // must not prevent an otherwise decodable image from opening.
    let (source_width, source_height) = if (5..=8).contains(&raw_source.get_orientation()) {
        (raw_source.get_height(), raw_source.get_width())
    } else {
        (raw_source.get_width(), raw_source.get_height())
    };
    let dpi = raw_source.get_xres() * 25.4;
    let source_dpi = (36.0..=2400.0).contains(&dpi).then_some(dpi);
    let file_type = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_uppercase())
        .unwrap_or_else(|| "Unknown".to_string());
    let metadata = describe_metadata(path, &raw_source, &file_type);
    let options = ops::ThumbnailOptions {
        height: DECODE_HEIGHT,
        size: ops::Size::Down,
        ..Default::default()
    };
    let thumbnail =
        ops::thumbnail_with_opts(filename, DECODE_WIDTH, &options).map_err(|e| e.to_string())?;
    let srgb =
        ops::colourspace(&thumbnail, ops::Interpretation::Srgb).map_err(|e| e.to_string())?;
    let rgba = ensure_rgba(srgb)?;
    let rgba = ops::cast(&rgba, ops::BandFormat::Uchar).map_err(|e| e.to_string())?;
    let width = rgba.get_width();
    let height = rgba.get_height();
    let stride = width as usize * 4;
    let pixels = rgba.image_write_to_memory();
    if pixels.len() != stride * height as usize {
        return Err(format!(
            "unexpected decoded buffer size: got {}, expected {}",
            pixels.len(),
            stride * height as usize
        ));
    }
    Ok(DecodedFrame {
        path: path.to_path_buf(),
        width,
        height,
        stride,
        pixels: pixels.into(),
        source_width,
        source_height,
        source_dpi,
        file_type,
        metadata,
    })
}

fn describe_metadata(path: &Path, image: &VipsImage, file_type: &str) -> String {
    let mut lines = vec![
        format!(
            "File\n{}\n",
            path.file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("image")
        ),
        format!("Type: {file_type}"),
        format!("Dimensions: {} × {}", image.get_width(), image.get_height()),
        format!("Bands: {}", image.get_bands()),
    ];
    if let Ok(file) = std::fs::metadata(path) {
        lines.push(format!("File size: {}", human_size(file.len())));
    }
    lines.push(format!(
        "Resolution: {:.2} × {:.2} px/mm",
        image.get_xres(),
        image.get_yres()
    ));
    if image.get_n_pages() > 1 {
        lines.push(format!("Pages: {}", image.get_n_pages()));
    }
    if image.get_orientation() > 1 {
        lines.push(format!("Orientation: {}", image.get_orientation()));
    }

    const EXIF_FIELDS: &[(&str, &str)] = &[
        ("Camera maker", "exif-ifd0-Make"),
        ("Camera model", "exif-ifd0-Model"),
        ("Software", "exif-ifd0-Software"),
        ("Captured", "exif-ifd0-DateTime"),
        ("Artist", "exif-ifd0-Artist"),
        ("Copyright", "exif-ifd0-Copyright"),
        ("Exposure", "exif-ifd2-ExposureTime"),
        ("Aperture", "exif-ifd2-FNumber"),
        ("ISO", "exif-ifd2-ISOSpeedRatings"),
        ("Focal length", "exif-ifd2-FocalLength"),
        ("Lens maker", "exif-ifd2-LensMake"),
        ("Lens", "exif-ifd2-LensModel"),
        ("Flash", "exif-ifd2-Flash"),
        ("GPS latitude", "exif-ifd3-GPSLatitude"),
        ("GPS longitude", "exif-ifd3-GPSLongitude"),
    ];
    let exif: Vec<String> = EXIF_FIELDS
        .iter()
        .filter_map(|(label, field)| {
            image
                .get_as_string(field)
                .ok()
                .map(|value| format!("{label}: {value}"))
        })
        .collect();
    if !exif.is_empty() {
        lines.push("\nEXIF".to_string());
        lines.extend(exif);
    }
    lines.join("\n")
}

fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn ensure_rgba(image: VipsImage) -> Result<VipsImage, String> {
    match image.get_bands() {
        4 => Ok(image),
        3 => ops::addalpha(&image).map_err(|e| e.to_string()),
        bands => Err(format!("unsupported decoded band count: {bands}")),
    }
}

fn sibling_images(initial_path: &Path) -> Result<Vec<PathBuf>, String> {
    if !initial_path.is_file() {
        return Err(format!("not a file: {}", initial_path.display()));
    }
    let parent = initial_path
        .parent()
        .ok_or_else(|| "image has no parent directory".to_string())?;
    let mut paths: Vec<PathBuf> = parent
        .read_dir()
        .map_err(|error| format!("cannot read {}: {error}", parent.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && is_supported_image(path))
        .collect();
    paths.sort_by_cached_key(|path| {
        natural_sort_key(
            &path
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default(),
        )
    });
    if paths.is_empty() {
        return Err(format!("no supported images in {}", parent.display()));
    }
    Ok(paths)
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum NaturalChunk {
    // Digit runs with leading zeros stripped: comparing stripped length first
    // and then the digit string orders runs by numeric value without a parse
    // that could overflow.
    Number(usize, String),
    Text(String),
}

/// Sort key matching file managers' natural order: case-insensitive, with
/// digit runs compared as numbers so "img_9" sorts before "img_10". The full
/// name tiebreaks entries whose chunks compare equal ("img01" vs "img1").
fn natural_sort_key(name: &str) -> (Vec<NaturalChunk>, String) {
    let lower = name.to_lowercase();
    let mut chunks = Vec::new();
    let mut rest = lower.as_str();
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let (run, tail) = if digits > 0 {
            let (run, tail) = rest.split_at(digits);
            let run = run.trim_start_matches('0');
            (NaturalChunk::Number(run.len(), run.to_string()), tail)
        } else {
            let end = rest
                .find(|c: char| c.is_ascii_digit())
                .unwrap_or(rest.len());
            let (run, tail) = rest.split_at(end);
            (NaturalChunk::Text(run.to_string()), tail)
        };
        chunks.push(run);
        rest = tail;
    }
    (chunks, name.to_string())
}

fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "avif"
                    | "bmp"
                    | "gif"
                    | "heic"
                    | "heif"
                    | "jpeg"
                    | "jpg"
                    | "jxl"
                    | "png"
                    | "tif"
                    | "tiff"
                    | "webp"
            )
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(name: &str, size: usize) -> DecodedFrame {
        DecodedFrame {
            path: PathBuf::from(name),
            width: size as i32 / 4,
            height: 1,
            stride: size,
            pixels: vec![0; size].into(),
            source_width: size as i32 / 4,
            source_height: 1,
            source_dpi: None,
            file_type: "TEST".to_string(),
            metadata: String::new(),
        }
    }

    #[test]
    fn default_zoom_never_enlarges_small_images() {
        assert_eq!(default_zoom(1200, 800, 400, 300), 1.0);
    }

    #[test]
    fn default_zoom_shrinks_large_images_to_fit() {
        assert_eq!(default_zoom(1200, 800, 2400, 1200), 0.5);
        assert_eq!(default_zoom(1200, 800, 1200, 1600), 0.5);
    }

    #[test]
    fn zoom_steps_use_clean_source_relative_levels() {
        assert_eq!(next_zoom_level(0.63), 0.667);
        assert_eq!(next_zoom_level(0.75), 1.0);
        assert_eq!(next_zoom_level(4.0), 5.0);
        assert_eq!(next_zoom_level(32.0), 32.0);
    }

    #[test]
    fn zooming_out_visits_fit_between_standard_levels() {
        assert_eq!(previous_zoom_level(0.667, 0.63), None);
        assert_eq!(previous_zoom_level(1.0, 0.63), Some(0.75));
        assert_eq!(previous_zoom_level(2.0, 0.8), Some(1.5));
    }

    #[test]
    fn zoom_percentage_keeps_useful_fractional_levels() {
        assert_eq!(format_zoom(1.0), "100%");
        assert_eq!(format_zoom(0.667), "66.7%");
        assert_eq!(format_zoom(0.125), "12.5%");
    }

    #[test]
    fn scaled_image_size_preserves_aspect_ratio() {
        let (width, height) = scaled_image_size(1600, 900, 1.25);
        assert!((width / height - 1600.0 / 900.0).abs() < 0.0001);
    }

    #[test]
    fn formats_file_sizes_for_metadata() {
        assert_eq!(human_size(900), "900 B");
        assert_eq!(human_size(1536), "1.5 KiB");
    }

    #[test]
    fn print_fit_preserves_aspect_ratio_and_margins() {
        let spec = PrintSpec {
            paper_width_mm: 210.0,
            paper_height_mm: 297.0,
            fit_to_page: true,
            image_dpi: 300.0,
            align_x: 0.5,
            align_y: 0.5,
            offset_x_mm: 0.0,
            offset_y_mm: 0.0,
        };
        let (_, _, width, height) = spec.image_rect_points(2000, 1000);
        assert!((width / height - 2.0).abs() < 0.0001);
        assert!(width < 210.0 / 25.4 * 72.0);
    }

    #[test]
    fn print_dpi_controls_physical_size() {
        let spec = PrintSpec {
            paper_width_mm: 210.0,
            paper_height_mm: 297.0,
            fit_to_page: false,
            image_dpi: 300.0,
            align_x: 0.5,
            align_y: 0.5,
            offset_x_mm: 0.0,
            offset_y_mm: 0.0,
        };
        let (_, _, width, height) = spec.image_rect_points(3000, 1500);
        assert!((width - 720.0).abs() < 0.0001);
        assert!((height - 360.0).abs() < 0.0001);
    }

    #[test]
    fn cache_evicts_the_least_recently_used_unprotected_frame() {
        let mut cache = FrameCache::new(8);
        cache.insert(frame("first", 4), Path::new("first"));
        cache.insert(frame("second", 4), Path::new("second"));

        assert!(cache.get(Path::new("first")).is_some());
        cache.insert(frame("third", 4), Path::new("third"));

        assert!(cache.contains(Path::new("first")));
        assert!(!cache.contains(Path::new("second")));
        assert!(cache.contains(Path::new("third")));
        assert_eq!(cache.used_bytes, 8);
    }

    #[test]
    fn cache_keeps_the_current_frame_when_over_budget() {
        let mut cache = FrameCache::new(4);
        cache.insert(frame("current", 8), Path::new("current"));
        cache.insert(frame("prefetch", 4), Path::new("current"));

        assert!(cache.contains(Path::new("current")));
        assert!(!cache.contains(Path::new("prefetch")));
        assert_eq!(cache.used_bytes, 8);
    }

    #[test]
    fn edited_dimensions_rotate_and_preserve_aspect_ratio() {
        assert_eq!(edited_dimensions(4000, 3000, 0, 2000), (2000, 1500));
        assert_eq!(edited_dimensions(4000, 3000, 1, 2000), (1500, 2000));
        assert_eq!(edited_dimensions(4000, 3000, 2, 5000), (4000, 3000));
    }

    #[test]
    fn sorts_folder_listing_naturally() {
        let mut names = vec![
            "IMG_10.jpg",
            "img_9.jpg",
            "IMG_2.jpg",
            "img_01.jpg",
            "img_1.jpg",
            "cover.png",
        ];
        names.sort_by_cached_key(|name| natural_sort_key(name));
        assert_eq!(
            names,
            vec![
                "cover.png",
                "img_01.jpg",
                "img_1.jpg",
                "IMG_2.jpg",
                "img_9.jpg",
                "IMG_10.jpg",
            ]
        );
    }

    #[test]
    fn discovers_and_decodes_the_sample_image() {
        let sample = PathBuf::from("test/flower.jpg")
            .canonicalize()
            .expect("sample image should exist");
        let paths = sibling_images(&sample).expect("sample directory should be readable");
        assert_eq!(paths, vec![sample.clone()]);

        let _vips = VipsApp::new("better-image-view-test", false)
            .expect("libvips should initialize for the decode test");
        let frame = decode_thumbnail(&sample).expect("sample image should decode");
        assert!(frame.width > 0);
        assert!(frame.height > 0);
        assert_eq!(
            frame.pixels.len(),
            frame.width as usize * frame.height as usize * 4
        );
    }
}
