//! Kitty graphics images on screen: placeholder runs from the snapshot become clipped slices
//! of the image, painted under the text.
//!
//! Each image is uploaded once per pixel change (the store's per-image generation) and freed
//! from the sprite atlas when the program deletes or replaces it.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;

use gpui_kit::{Bounds, Pixels, Point, RenderImage, Window, point, px, size};
use maomi_term::{GridSnapshot, KittyImage, PtySession};

/// GPU copies of one session's images.
#[derive(Debug, Default)]
pub(crate) struct ImageCache {
    /// The session these ids belong to (its address); a new session starts a new cache.
    session: usize,
    /// The store generation last checked against.
    generation: Option<u64>,
    entries: HashMap<u32, Cached>,
}

#[derive(Debug)]
struct Cached {
    /// The store's generation for this image when it was uploaded.
    generation: u64,
    image: Arc<RenderImage>,
    width: u32,
    height: u32,
    columns: u32,
    rows: u32,
}

/// One slice to paint: the whole placement's image bounds, clipped to a run of cells.
pub(crate) struct ImagePaint {
    pub clip: Bounds<Pixels>,
    pub image_bounds: Bounds<Pixels>,
    pub image: Arc<RenderImage>,
}

impl ImageCache {
    /// Drops every GPU copy whose image the program replaced or deleted.
    fn sync(&mut self, session: &Arc<PtySession>, window: &mut Window) {
        let key = Arc::as_ptr(session) as usize;
        if key != self.session {
            self.drop_all(window);
            self.session = key;
        }
        let generation = session.images_generation();
        if self.generation == Some(generation) {
            return;
        }
        self.generation = Some(generation);
        let stale: Vec<u32> = self
            .entries
            .iter()
            .filter(|(id, cached)| {
                session
                    .image(**id)
                    .is_none_or(|image| image.generation != cached.generation)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            if let Some(cached) = self.entries.remove(&id) {
                let _ = window.drop_image(cached.image);
            }
        }
    }

    /// The uploaded copy of image `id`, shared with the preview so it needs no second upload.
    pub(crate) fn render_image(&self, id: u32) -> Option<Arc<RenderImage>> {
        self.entries.get(&id).map(|cached| cached.image.clone())
    }

    fn drop_all(&mut self, window: &mut Window) {
        for (_, cached) in self.entries.drain() {
            let _ = window.drop_image(cached.image);
        }
        self.generation = None;
    }

    fn get_or_upload(&mut self, id: u32, session: &PtySession) -> Option<&Cached> {
        match self.entries.entry(id) {
            Entry::Occupied(entry) => Some(entry.into_mut()),
            Entry::Vacant(entry) => Some(entry.insert(upload(&session.image(id)?))),
        }
    }
}

fn upload(image: &KittyImage) -> Cached {
    // GPUI's atlas takes BGRA.
    let mut bgra = image.rgba.as_ref().clone();
    for pixel in bgra.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let buffer = ::image::RgbaImage::from_raw(image.width, image.height, bgra)
        .expect("the store keeps width * height * 4 bytes");
    Cached {
        generation: image.generation,
        image: Arc::new(RenderImage::new(vec![::image::Frame::new(buffer)])),
        width: image.width,
        height: image.height,
        columns: image.columns,
        rows: image.rows,
    }
}

/// The slices to paint for the snapshot's placeholder runs.
pub(crate) fn layout_images(
    cache: &mut ImageCache,
    session: &Arc<PtySession>,
    snapshot: &GridSnapshot,
    cell_origin: impl Fn(usize, usize) -> Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    window: &mut Window,
) -> Vec<ImagePaint> {
    let runs = snapshot.image_runs();
    if runs.is_empty() && cache.entries.is_empty() {
        return Vec::new();
    }
    cache.sync(session, window);
    let scale = window.scale_factor();
    let mut paints = Vec::with_capacity(runs.len());
    for run in runs {
        let Some(cached) = cache.get_or_upload(run.image_id, session) else {
            continue;
        };
        // Without c / r the placement is the image's own size in cells, as in kitty.
        let device_cell = (f32::from(cell_width) * scale, f32::from(line_height) * scale);
        let columns = match cached.columns {
            0 => (cached.width as f32 / device_cell.0).ceil().max(1.),
            c => c as f32,
        };
        let rows = match cached.rows {
            0 => (cached.height as f32 / device_cell.1).ceil().max(1.),
            r => r as f32,
        };
        // Fit the image into the placement's box keeping its aspect ratio, centred.
        let box_size = size(cell_width * columns, line_height * rows);
        let fit = (f32::from(box_size.width) / cached.width as f32)
            .min(f32::from(box_size.height) / cached.height as f32);
        let drawn = size(
            px(cached.width as f32 * fit),
            px(cached.height as f32 * fit),
        );
        let run_origin = cell_origin(run.row, run.col);
        let placement_origin = point(
            run_origin.x - cell_width * run.image_col as f32,
            run_origin.y - line_height * run.image_row as f32,
        );
        let image_origin = point(
            placement_origin.x + (box_size.width - drawn.width) / 2.,
            placement_origin.y + (box_size.height - drawn.height) / 2.,
        );
        paints.push(ImagePaint {
            clip: Bounds::new(run_origin, size(cell_width * run.len as f32, line_height)),
            image_bounds: Bounds::new(image_origin, drawn),
            image: cached.image.clone(),
        });
    }
    paints
}
