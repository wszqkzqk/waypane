use std::sync::Arc;

use thiserror::Error;
use waypane_core::{BufferView, Frame, Point, Rect, ShmFormat, Size, SurfaceId};

#[derive(Debug, Clone)]
pub struct DecodedBuffer {
    pub size: Size,
    pub rgba: Arc<[u8]>,
}

#[derive(Debug, Error)]
pub enum ImageError {
    #[error("buffer access out of bounds: {0}")]
    OutOfBounds(String),
}

#[derive(Debug, Clone)]
pub struct UploadRegion {
    pub rect: Rect,
    pub rgba: Vec<u8>,
}

#[derive(Debug)]
struct Layer {
    size: Size,
    offset: Point,
    rgba: Vec<u8>,
    opaque: bool,
}

#[derive(Debug, Default)]
pub struct Canvas {
    size: Size,
    rgba: Vec<u8>,
    layers: std::collections::BTreeMap<SurfaceId, Layer>,
    order: Vec<SurfaceId>,
}

impl Canvas {
    pub fn size(&self) -> Size {
        self.size
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    pub fn apply_frame(&mut self, frame: &Frame) -> Result<Vec<UploadRegion>, ImageError> {
        if let Some(order) = &frame.z_order {
            self.order.clone_from(order);
        }
        let Some(view) = frame.buffer.as_ref() else {
            return Ok(self.remove_layer(frame.surface_id));
        };
        let size = output_size(view.width as u32, view.height as u32, view.transform);
        let offset = frame.parent.map(|(_, point)| point).unwrap_or_default();
        let is_root = frame.parent.is_none();
        let recreate = self
            .layers
            .get(&frame.surface_id)
            .is_none_or(|layer| layer.size != size);

        let mut dirty = Vec::new();
        if let Some(previous) = self.layers.get(&frame.surface_id) {
            if previous.size != size || previous.offset != offset {
                dirty.push(Rect {
                    x: previous.offset.x,
                    y: previous.offset.y,
                    width: previous.size.width as i32,
                    height: previous.size.height as i32,
                });
            }
        }

        let source_damage = if recreate {
            vec![Rect {
                x: 0,
                y: 0,
                width: view.width,
                height: view.height,
            }]
        } else {
            view.damage.clone()
        };
        let decoded = decode_regions(&view.data, view, &source_damage)?;
        let pixel_bytes = (size.width as usize)
            .checked_mul(size.height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| {
                ImageError::OutOfBounds(format!(
                    "layer size {}x{} overflows addressable memory",
                    size.width, size.height
                ))
            })?;
        if recreate
            || self
                .layers
                .get(&frame.surface_id)
                .is_some_and(|layer| layer.rgba.len() != pixel_bytes)
        {
            self.layers.insert(
                frame.surface_id,
                Layer {
                    size,
                    offset,
                    rgba: vec![0; pixel_bytes],
                    opaque: view.format == ShmFormat::Xrgb8888,
                },
            );
        }
        let layer = self
            .layers
            .get_mut(&frame.surface_id)
            .ok_or_else(|| ImageError::OutOfBounds("layer was not allocated".to_string()))?;
        layer.size = size;
        layer.offset = offset;
        layer.opaque = view.format == ShmFormat::Xrgb8888;
        for region in &decoded {
            blit_rgba(
                &mut layer.rgba,
                size.width as usize,
                region.rect,
                &region.rgba,
            );
            dirty.push(Rect {
                x: offset.x + region.rect.x,
                y: offset.y + region.rect.y,
                width: region.rect.width,
                height: region.rect.height,
            });
        }
        if !self.order.contains(&frame.surface_id) {
            if is_root {
                self.order.insert(0, frame.surface_id);
            } else {
                self.order.push(frame.surface_id);
            }
        }

        if is_root && self.size != size {
            self.size = size;
            self.rgba
                .resize(size.width as usize * size.height as usize * 4, 0);
            dirty.push(Rect {
                x: 0,
                y: 0,
                width: size.width as i32,
                height: size.height as i32,
            });
        }
        Ok(self.compose_regions(&dirty))
    }

    pub fn remove_layer(&mut self, surface_id: SurfaceId) -> Vec<UploadRegion> {
        let Some(layer) = self.layers.remove(&surface_id) else {
            return Vec::new();
        };
        self.order.retain(|id| *id != surface_id);
        self.compose_regions(&[Rect {
            x: layer.offset.x,
            y: layer.offset.y,
            width: layer.size.width as i32,
            height: layer.size.height as i32,
        }])
    }

    fn compose_regions(&mut self, dirty: &[Rect]) -> Vec<UploadRegion> {
        let Some(rect) = bounding_rect(dirty).and_then(|rect| clip(rect, self.size)) else {
            return Vec::new();
        };
        let mut rgba = subimage(&self.rgba, self.size, rect);
        let opaque_root = self.order.len() == 1
            && self.layers.get(&self.order[0]).is_some_and(|layer| {
                let covers = layer.offset.x <= rect.x
                    && layer.offset.y <= rect.y
                    && layer.offset.x + layer.size.width as i32 >= rect.x + rect.width
                    && layer.offset.y + layer.size.height as i32 >= rect.y + rect.height;
                layer.opaque && covers
            });
        if opaque_root {
            let layer = &self.layers[&self.order[0]];
            for row in 0..rect.height as usize {
                let dst = row * rect.width as usize * 4;
                let src_row = (rect.y + row as i32 - layer.offset.y) as usize;
                let src = src_row * layer.size.width as usize * 4
                    + (rect.x - layer.offset.x) as usize * 4;
                rgba[dst..dst + rect.width as usize * 4]
                    .copy_from_slice(&layer.rgba[src..src + rect.width as usize * 4]);
            }
        } else {
            rgba.fill(0);
            for id in &self.order {
                let Some(layer) = self.layers.get(id) else {
                    continue;
                };
                blend_region(&mut rgba, rect, layer);
            }
        }
        blit_rgba(&mut self.rgba, self.size.width as usize, rect, &rgba);
        vec![UploadRegion { rect, rgba }]
    }
}

pub fn decode_buffer(view: &BufferView) -> Result<DecodedBuffer, ImageError> {
    if view.width <= 0 || view.height <= 0 || view.offset < 0 {
        return Err(ImageError::OutOfBounds(
            "cursor buffer has invalid dimensions or offset".to_string(),
        ));
    }
    let size = output_size(view.width as u32, view.height as u32, view.transform);
    let regions = decode_regions(
        &view.data,
        view,
        &[Rect {
            x: 0,
            y: 0,
            width: view.width,
            height: view.height,
        }],
    )?;
    let region = regions.into_iter().next().ok_or_else(|| {
        ImageError::OutOfBounds("cursor buffer decoded to an empty region".to_string())
    })?;
    Ok(DecodedBuffer {
        size,
        rgba: Arc::from(region.rgba),
    })
}

fn decode_regions(
    pool: &[u8],
    view: &BufferView,
    damage: &[Rect],
) -> Result<Vec<UploadRegion>, ImageError> {
    damage
        .iter()
        .filter_map(|rect| {
            clip(
                *rect,
                Size {
                    width: view.width as u32,
                    height: view.height as u32,
                },
            )
        })
        .map(|source| {
            let rect = transform_rect(
                source,
                view.width as u32,
                view.height as u32,
                view.transform,
            );
            let mut rgba = vec![0u8; rect.width as usize * rect.height as usize * 4];
            for dy in 0..rect.height {
                for dx in 0..rect.width {
                    let (sx, sy) = inverse_transform_point(
                        rect.x + dx,
                        rect.y + dy,
                        view.width as u32,
                        view.height as u32,
                        view.transform,
                    );
                    let offset =
                        view.offset as usize + sy as usize * view.stride as usize + sx as usize * 4;
                    let pixel = pool.get(offset..offset + 4).ok_or_else(|| {
                        ImageError::OutOfBounds(format!(
                            "pixel ({sx}, {sy}) at byte {offset} exceeds pool of {} bytes",
                            pool.len()
                        ))
                    })?;
                    let dst = ((dy * rect.width + dx) * 4) as usize;
                    rgba[dst] = pixel[2];
                    rgba[dst + 1] = pixel[1];
                    rgba[dst + 2] = pixel[0];
                    rgba[dst + 3] = match view.format {
                        ShmFormat::Argb8888 => pixel[3],
                        ShmFormat::Xrgb8888 => 255,
                    };
                }
            }
            Ok(UploadRegion { rect, rgba })
        })
        .collect()
}

pub fn output_size(width: u32, height: u32, transform: u32) -> Size {
    if transform % 2 == 1 {
        Size {
            width: height,
            height: width,
        }
    } else {
        Size { width, height }
    }
}

fn transform_rect(source: Rect, width: u32, height: u32, transform: u32) -> Rect {
    let x2 = source.x + source.width - 1;
    let y2 = source.y + source.height - 1;
    let points = [
        transform_point(source.x, source.y, width, height, transform),
        transform_point(x2, source.y, width, height, transform),
        transform_point(source.x, y2, width, height, transform),
        transform_point(x2, y2, width, height, transform),
    ];
    let min_x = points.iter().map(|point| point.0).min().unwrap();
    let max_x = points.iter().map(|point| point.0).max().unwrap();
    let min_y = points.iter().map(|point| point.1).min().unwrap();
    let max_y = points.iter().map(|point| point.1).max().unwrap();
    Rect {
        x: min_x,
        y: min_y,
        width: max_x - min_x + 1,
        height: max_y - min_y + 1,
    }
}

fn transform_point(x: i32, y: i32, width: u32, height: u32, transform: u32) -> (i32, i32) {
    let w = width as i32;
    let h = height as i32;
    match transform {
        0 => (x, y),
        1 => (h - 1 - y, x),
        2 => (w - 1 - x, h - 1 - y),
        3 => (y, w - 1 - x),
        4 => (w - 1 - x, y),
        5 => (h - 1 - y, w - 1 - x),
        6 => (x, h - 1 - y),
        7 => (y, x),
        _ => (x, y),
    }
}

fn inverse_transform_point(x: i32, y: i32, width: u32, height: u32, transform: u32) -> (i32, i32) {
    let w = width as i32;
    let h = height as i32;
    match transform {
        0 => (x, y),
        1 => (y, h - 1 - x),
        2 => (w - 1 - x, h - 1 - y),
        3 => (w - 1 - y, x),
        4 => (w - 1 - x, y),
        5 => (w - 1 - y, h - 1 - x),
        6 => (x, h - 1 - y),
        7 => (y, x),
        _ => (x, y),
    }
}

fn blit_rgba(dst: &mut [u8], dst_width: usize, region: Rect, rgba: &[u8]) {
    for row in 0..region.height as usize {
        let dst_offset = ((region.y as usize + row) * dst_width + region.x as usize) * 4;
        let src_offset = row * region.width as usize * 4;
        let len = region.width as usize * 4;
        dst[dst_offset..dst_offset + len].copy_from_slice(&rgba[src_offset..src_offset + len]);
    }
}

fn blend_region(canvas: &mut [u8], rect: Rect, layer: &Layer) {
    for y in rect.y..rect.y + rect.height {
        for x in rect.x..rect.x + rect.width {
            let lx = x - layer.offset.x;
            let ly = y - layer.offset.y;
            if lx < 0 || ly < 0 || lx >= layer.size.width as i32 || ly >= layer.size.height as i32 {
                continue;
            }
            let src = (ly as usize * layer.size.width as usize + lx as usize) * 4;
            let dst = (((y - rect.y) as usize * rect.width as usize) + (x - rect.x) as usize) * 4;
            let alpha = u32::from(layer.rgba[src + 3]);
            if alpha == 0 {
                continue;
            }
            let inverse = 255 - alpha;
            for channel in 0..3 {
                let value = u32::from(layer.rgba[src + channel]) * alpha
                    + u32::from(canvas[dst + channel]) * inverse;
                canvas[dst + channel] = (value / 255) as u8;
            }
            let background_alpha = u32::from(canvas[dst + 3]);
            canvas[dst + 3] = ((background_alpha * inverse + 255 * alpha) / 255) as u8;
        }
    }
}

fn subimage(rgba: &[u8], size: Size, rect: Rect) -> Vec<u8> {
    let mut output = vec![0; rect.width as usize * rect.height as usize * 4];
    for row in 0..rect.height as usize {
        let src = ((rect.y as usize + row) * size.width as usize + rect.x as usize) * 4;
        let dst = row * rect.width as usize * 4;
        let len = rect.width as usize * 4;
        output[dst..dst + len].copy_from_slice(&rgba[src..src + len]);
    }
    output
}

fn bounding_rect(regions: &[Rect]) -> Option<Rect> {
    let mut iter = regions
        .iter()
        .filter(|rect| rect.width > 0 && rect.height > 0);
    let first = *iter.next()?;
    iter.fold(first, |bounds, rect| {
        let x1 = bounds.x.min(rect.x);
        let y1 = bounds.y.min(rect.y);
        let x2 = (bounds.x + bounds.width).max(rect.x + rect.width);
        let y2 = (bounds.y + bounds.height).max(rect.y + rect.height);
        Rect {
            x: x1,
            y: y1,
            width: x2 - x1,
            height: y2 - y1,
        }
    })
    .into()
}

fn clip(rect: Rect, size: Size) -> Option<Rect> {
    let x1 = rect.x.clamp(0, size.width as i32);
    let y1 = rect.y.clamp(0, size.height as i32);
    let x2 = (rect.x + rect.width).clamp(0, size.width as i32);
    let y2 = (rect.y + rect.height).clamp(0, size.height as i32);
    (x2 > x1 && y2 > y1).then_some(Rect {
        x: x1,
        y: y1,
        width: x2 - x1,
        height: y2 - y1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(width: i32, height: i32, transform: u32) -> BufferView {
        BufferView {
            rid: waypipe_client::Rid(1),
            data: waypipe_client::ShmData::default(),
            offset: 0,
            width,
            height,
            stride: width * 4,
            format: ShmFormat::Argb8888,
            scale: 1,
            transform,
            damage: vec![Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            }],
        }
    }

    fn frame(surface_id: SurfaceId, buffer: BufferView, parent: Option<(u32, Point)>) -> Frame {
        Frame {
            id: 1,
            window_id: 1,
            surface_id,
            parent,
            buffer: Some(buffer),
            z_order: None,
        }
    }

    #[test]
    fn overlapping_translucent_layers_use_source_over_alpha() {
        let mut canvas = Canvas::default();
        let mut bottom = view(1, 1, 0);
        bottom.damage = vec![Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        }];
        bottom.data = vec![0_u8, 0, 255, 128].into();
        canvas.apply_frame(&frame(1, bottom.clone(), None)).unwrap();
        let mut top = bottom;
        top.data = vec![255_u8, 0, 0, 128].into();
        canvas
            .apply_frame(&frame(2, top, Some((1, Point { x: 0, y: 0 }))))
            .unwrap();
        let pixel = &canvas.rgba()[..4];
        assert_eq!(pixel[3], 191, "source-over alpha of two 128/255 layers");
        assert!(
            pixel[0].abs_diff(63) <= 1 && pixel[2].abs_diff(128) <= 1,
            "premultiplied rgb: {pixel:?}"
        );
    }

    #[test]
    fn converts_xrgb_and_updates_only_damage() {
        let mut view = view(2, 2, 0);
        view.format = ShmFormat::Xrgb8888;
        let mut pool = vec![1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0, 10, 11, 12, 0];
        view.data = pool.clone().into();
        let mut canvas = Canvas::default();
        let uploads = canvas.apply_frame(&frame(1, view.clone(), None)).unwrap();
        assert_eq!(uploads.len(), 1);
        assert_eq!(
            uploads[0].rect,
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2
            }
        );
        assert_eq!(&uploads[0].rgba[..8], &[3, 2, 1, 255, 6, 5, 4, 255]);

        pool[4..8].copy_from_slice(&[20, 21, 22, 0]);
        view.data = pool.into();
        view.damage = vec![Rect {
            x: 1,
            y: 0,
            width: 1,
            height: 1,
        }];
        let uploads = canvas.apply_frame(&frame(1, view, None)).unwrap();
        assert_eq!(
            uploads[0].rect,
            Rect {
                x: 1,
                y: 0,
                width: 1,
                height: 1
            }
        );
        assert_eq!(uploads[0].rgba, vec![22, 21, 20, 255]);
    }

    #[test]
    fn blends_subsurface_over_root() {
        let mut root_view = view(1, 1, 0);
        root_view.format = ShmFormat::Xrgb8888;
        root_view.data = [0, 200, 0, 0].to_vec().into();
        let mut canvas = Canvas::default();
        canvas.apply_frame(&frame(1, root_view, None)).unwrap();

        let mut child_view = view(1, 1, 0);
        child_view.data = [0, 0, 255, 128].to_vec().into();
        let uploads = canvas
            .apply_frame(&frame(2, child_view, Some((1, Point { x: 0, y: 0 }))))
            .unwrap();
        assert_eq!(uploads[0].rgba, vec![128, 99, 0, 255]);
    }

    #[test]
    fn applies_rotation_and_flips() {
        let mut view = view(2, 1, 1);
        view.data = [0, 0, 255, 255, 0, 255, 0, 255].to_vec().into();
        let mut canvas = Canvas::default();
        let uploads = canvas.apply_frame(&frame(1, view, None)).unwrap();
        assert_eq!(
            canvas.size(),
            Size {
                width: 1,
                height: 2
            }
        );
        assert_eq!(uploads[0].rgba, vec![255, 0, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn reallocates_layer_pixels_across_repeated_root_resizes() {
        let mut canvas = Canvas::default();
        for (width, height) in [(1, 1), (2, 2), (1, 2), (3, 1), (1, 1)] {
            let mut view = view(width, height, 0);
            view.format = ShmFormat::Xrgb8888;
            view.data = [10, 20, 30, 0]
                .into_iter()
                .cycle()
                .take((width * height * 4) as usize)
                .collect::<Vec<_>>()
                .into();
            canvas.apply_frame(&frame(1, view, None)).unwrap();
            assert_eq!(
                canvas.size(),
                Size {
                    width: width as u32,
                    height: height as u32,
                }
            );
            assert_eq!(canvas.rgba().len(), (width * height * 4) as usize);
            assert!(canvas
                .rgba()
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| *pixel == [30, 20, 10, 255]));
        }
    }
}
