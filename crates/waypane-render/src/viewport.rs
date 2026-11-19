use waypane_core::Size;

pub const MIN_VIEWPORT_ZOOM: f32 = 1.0;
pub const MAX_VIEWPORT_ZOOM: f32 = 8.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub zoom: f32,
    pub pan_x: f32,
    pub pan_y: f32,
}

impl Viewport {
    pub fn sanitize(self) -> Self {
        let zoom = if self.zoom.is_nan() {
            MIN_VIEWPORT_ZOOM
        } else {
            self.zoom
        };
        Self {
            zoom: zoom.clamp(MIN_VIEWPORT_ZOOM, MAX_VIEWPORT_ZOOM),
            pan_x: finite_or_zero(self.pan_x),
            pan_y: finite_or_zero(self.pan_y),
        }
    }
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            zoom: MIN_VIEWPORT_ZOOM,
            pan_x: 0.0,
            pan_y: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewportTransform {
    viewport: Viewport,
    draw_x: f32,
    draw_y: f32,
    draw_width: f32,
    draw_height: f32,
}

impl ViewportTransform {
    pub fn new(buffer_size: Size, view_size: Size, viewport: Viewport) -> Option<Self> {
        if buffer_size.width == 0
            || buffer_size.height == 0
            || view_size.width == 0
            || view_size.height == 0
        {
            return None;
        }
        let viewport = viewport.sanitize();
        let buffer_aspect = buffer_size.width as f32 / buffer_size.height as f32;
        let view_aspect = view_size.width as f32 / view_size.height as f32;
        if !buffer_aspect.is_finite() || !view_aspect.is_finite() || view_aspect <= 0.0 {
            return None;
        }

        let (draw_width, draw_height) = if buffer_aspect >= view_aspect {
            (viewport.zoom, viewport.zoom * view_aspect / buffer_aspect)
        } else {
            (viewport.zoom * buffer_aspect / view_aspect, viewport.zoom)
        };
        let pan_x = clamp_pan(viewport.pan_x, draw_width);
        let pan_y = clamp_pan(viewport.pan_y, draw_height);
        Some(Self {
            viewport: Viewport {
                pan_x,
                pan_y,
                ..viewport
            },
            draw_x: (1.0 - draw_width) * 0.5 + pan_x,
            draw_y: (1.0 - draw_height) * 0.5 + pan_y,
            draw_width,
            draw_height,
        })
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    pub fn vertices(&self) -> [[f32; 4]; 6] {
        let left = self.draw_x * 2.0 - 1.0;
        let right = (self.draw_x + self.draw_width) * 2.0 - 1.0;
        let top = 1.0 - self.draw_y * 2.0;
        let bottom = 1.0 - (self.draw_y + self.draw_height) * 2.0;
        [
            [left, top, 0.0, 0.0],
            [left, bottom, 0.0, 1.0],
            [right, bottom, 1.0, 1.0],
            [left, top, 0.0, 0.0],
            [right, bottom, 1.0, 1.0],
            [right, top, 1.0, 0.0],
        ]
    }

    pub fn view_to_content(&self, x: f32, y: f32) -> Option<(f32, f32)> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let u = (x - self.draw_x) / self.draw_width;
        let v = (y - self.draw_y) / self.draw_height;
        ((0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v))
            .then_some((u.clamp(0.0, 1.0), v.clamp(0.0, 1.0)))
    }
}

fn clamp_pan(pan: f32, draw_size: f32) -> f32 {
    let limit = ((draw_size - 1.0) * 0.5).max(0.0);
    pan.clamp(-limit, limit)
}

fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1e-5,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn fits_landscape_desktop_inside_portrait_phone() {
        let transform = ViewportTransform::new(
            Size {
                width: 1920,
                height: 1080,
            },
            Size {
                width: 1220,
                height: 2254,
            },
            Viewport {
                zoom: 1.0,
                pan_x: 100.0,
                pan_y: -100.0,
            },
        )
        .unwrap();
        let vertices = transform.vertices();
        close(transform.draw_width, 1.0);
        close(transform.draw_height, 1220.0 / 2254.0 / (1920.0 / 1080.0));
        close(transform.draw_x, 0.0);
        close(transform.draw_y, (1.0 - transform.draw_height) * 0.5);
        assert!(vertices
            .iter()
            .all(|vertex| (-1.0..=1.0).contains(&vertex[0])));
        assert!(vertices
            .iter()
            .all(|vertex| (-1.0..=1.0).contains(&vertex[1])));
        let center = transform.view_to_content(0.5, 0.5).unwrap();
        close(center.0, 0.5);
        close(center.1, 0.5);
    }

    #[test]
    fn clamps_pan_to_keep_scaled_content_reachable() {
        let transform = ViewportTransform::new(
            Size {
                width: 1920,
                height: 1080,
            },
            Size {
                width: 1220,
                height: 2254,
            },
            Viewport {
                zoom: 4.0,
                pan_x: 100.0,
                pan_y: 100.0,
            },
        )
        .unwrap();
        let vertices = transform.vertices();
        let left = vertices
            .iter()
            .map(|vertex| vertex[0])
            .fold(f32::INFINITY, f32::min);
        let right = vertices
            .iter()
            .map(|vertex| vertex[0])
            .fold(f32::NEG_INFINITY, f32::max);
        let top = vertices
            .iter()
            .map(|vertex| vertex[1])
            .fold(f32::INFINITY, f32::min);
        let bottom = vertices
            .iter()
            .map(|vertex| vertex[1])
            .fold(f32::NEG_INFINITY, f32::max);
        close(left, -1.0);
        assert!(right > 1.0);
        assert!(bottom >= -1.0 && top <= 1.0);
        close(transform.pan_x_clamped_for_test(), 1.5);
        close(
            transform.pan_y_clamped_for_test(),
            (transform.draw_height - 1.0) * 0.5,
        );
    }

    #[test]
    fn fits_portrait_content_by_height_and_rejects_letterbox_touches() {
        let transform = ViewportTransform::new(
            Size {
                width: 600,
                height: 1200,
            },
            Size {
                width: 2712,
                height: 1220,
            },
            Viewport::default(),
        )
        .unwrap();
        close(transform.draw_height, 1.0);
        close(transform.draw_width, 0.5 / (2712.0 / 1220.0));
        assert_eq!(transform.view_to_content(0.1, 0.5), None);
        let center = transform.view_to_content(0.5, 0.5).unwrap();
        close(center.0, 0.5);
        close(center.1, 0.5);
        assert_eq!(transform.view_to_content(1.0, 0.5), None);
        assert_eq!(transform.view_to_content(f32::NAN, 0.5), None);
    }

    #[test]
    fn rejects_empty_sizes_and_sanitizes_non_finite_viewport_values() {
        let empty = Size {
            width: 0,
            height: 10,
        };
        let view = Size {
            width: 100,
            height: 100,
        };
        assert!(ViewportTransform::new(empty, view, Viewport::default()).is_none());
        let sanitized = Viewport {
            zoom: f32::INFINITY,
            pan_x: f32::NAN,
            pan_y: -0.25,
        }
        .sanitize();
        assert_eq!(sanitized.zoom, MAX_VIEWPORT_ZOOM);
        assert_eq!(sanitized.pan_x, 0.0);
        assert_eq!(sanitized.pan_y, -0.25);
        assert_eq!(
            Viewport {
                zoom: 0.25,
                pan_x: 0.0,
                pan_y: 0.0,
            }
            .sanitize()
            .zoom,
            MIN_VIEWPORT_ZOOM
        );
    }

    impl ViewportTransform {
        fn pan_x_clamped_for_test(&self) -> f32 {
            self.draw_x - (1.0 - self.draw_width) * 0.5
        }

        fn pan_y_clamped_for_test(&self) -> f32 {
            self.draw_y - (1.0 - self.draw_height) * 0.5
        }
    }
}
