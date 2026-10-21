/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Rasterization of the `clip-path` `<basic-shape>` values that WebRender has no
//! native clip primitive for: `polygon()`, `path()` and `shape()`.
//!
//! WebRender 0.70 can only clip with rectangles, rounded rectangles and
//! (rasterized) images. For every other shape we flatten the outline into closed
//! polygonal contours, rasterize them into a single-channel coverage bitmap and
//! hand that bitmap to WebRender as an *image mask* clip.

use std::f64::consts::{FRAC_PI_2, PI, SQRT_2};

use euclid::default::Size2D;
use style::values::computed::basic_shape::{BasicShape, PathOrShapeFunction, ShapeCommand};
use style::values::computed::position::Position;
use style::values::computed::{Length, LengthPercentage};
use style::values::generics::Optional;
use style::values::generics::basic_shape::{
    ArcSize, ArcSweep, AxisEndPoint, AxisPosition, CommandEndPoint, ControlPoint, ControlReference,
    FillRule, PolygonCoord,
};
use style::values::specified::svg_path::{PathCommand, SVGPathPosition};
use webrender_api::units::{LayoutPoint, LayoutRect};

/// A point in CSS pixels, relative to the origin of the shape's reference box.
type Point = (f64, f64);

/// Number of sub-scanlines sampled per device pixel row, to approximate vertical
/// antialiasing.
const SUBSAMPLES: u32 = 4;
/// Maximum distance (in CSS pixels) between a curve and its polyline approximation.
const FLATTEN_TOLERANCE: f64 = 0.1;
/// Recursion limit for the curve-flattening subdivision.
const MAX_FLATTEN_DEPTH: u32 = 16;

/// A cache mapping a rasterized shape (identified by a content hash) to the image
/// key it was uploaded with, so that repeated display-list builds reuse the same
/// WebRender image instead of leaking a new one per reflow.
pub(crate) type ShapeMaskImageCache = rustc_hash::FxHashMap<u64, webrender_api::ImageKey>;

/// The flattened outline of a `clip-path` `<basic-shape>` that must be drawn as
/// an image mask, plus the fill rule used to rasterize it.
#[derive(Clone, Debug)]
pub(crate) struct ClipShapeMask {
    contours: Vec<Vec<Point>>,
    fill_rule: FillRule,
}

impl ClipShapeMask {
    /// Build a rasterizable mask from a computed `<basic-shape>` that needs
    /// image-mask clipping (i.e. not a rect/circle/ellipse). `reference_size` is
    /// the size of the shape's reference box, in CSS pixels.
    pub(crate) fn from_basic_shape(
        shape: &BasicShape,
        reference_size: Size2D<f64>,
    ) -> Option<Self> {
        match shape {
            BasicShape::Polygon(polygon) => {
                let points: Vec<Point> = polygon
                    .coordinates
                    .iter()
                    .map(|PolygonCoord(x, y)| {
                        (
                            length_percentage_to_px(x, reference_size.width),
                            length_percentage_to_px(y, reference_size.height),
                        )
                    })
                    .collect();
                let radius = polygon
                    .round
                    .as_ref()
                    .map(|round| {
                        length_percentage_to_px(
                            round,
                            reference_size.width.min(reference_size.height),
                        )
                    })
                    .unwrap_or(0.0);
                let contours = if radius > 0.0 {
                    flatten_commands(&rounded_polygon_commands(&points, radius))
                } else {
                    vec![points]
                };
                Some(Self {
                    contours,
                    fill_rule: polygon.fill,
                })
            },
            BasicShape::PathOrShape(PathOrShapeFunction::Path(path)) => Some(Self {
                contours: flatten_commands(&lower_path_commands(path.commands(), reference_size)),
                fill_rule: path.fill,
            }),
            BasicShape::PathOrShape(PathOrShapeFunction::Shape(shape)) => Some(Self {
                contours: flatten_commands(&lower_shape_commands(shape.commands(), reference_size)),
                fill_rule: shape.fill,
            }),
            _ => None,
        }
    }

    /// The fill rule to use for WebRender hit-testing of the mask.
    pub(crate) fn webrender_fill_rule(&self) -> webrender_api::FillRule {
        match self.fill_rule {
            FillRule::Nonzero => webrender_api::FillRule::Nonzero,
            FillRule::Evenodd => webrender_api::FillRule::Evenodd,
        }
    }

    /// The flattened contours, in layout coordinates relative to `rect`, for
    /// WebRender clip hit-testing.
    pub(crate) fn hit_test_points(&self, rect: LayoutRect) -> Vec<LayoutPoint> {
        let origin = rect.min;
        let mut points = Vec::new();
        for contour in &self.contours {
            for &(x, y) in contour {
                points.push(LayoutPoint::new(origin.x + x as f32, origin.y + y as f32));
            }
        }
        points
    }

    /// A content hash identifying this mask's rendered output, used to dedupe
    /// uploaded images across display-list builds.
    pub(crate) fn cache_key(&self, width: u32, height: u32, scale: f32) -> u64 {
        use std::hash::{Hash, Hasher};

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let fill_rule = match self.fill_rule {
            FillRule::Nonzero => 0u8,
            FillRule::Evenodd => 1u8,
        };
        fill_rule.hash(&mut hasher);
        for contour in &self.contours {
            for &(x, y) in contour {
                x.to_bits().hash(&mut hasher);
                y.to_bits().hash(&mut hasher);
            }
            u32::MAX.hash(&mut hasher);
        }
        width.hash(&mut hasher);
        height.hash(&mut hasher);
        scale.to_bits().hash(&mut hasher);
        hasher.finish()
    }

    /// Rasterize the shape into an RGBA8 coverage bitmap of `width` x `height`
    /// device pixels. `scale` is the device pixel ratio; the shape's reference
    /// box is assumed to span the whole bitmap.
    pub(crate) fn rasterize(&self, width: u32, height: u32, scale: f64) -> Vec<u8> {
        let mut data = vec![0u8; (width as usize) * (height as usize) * 4];
        if width == 0 || height == 0 {
            return data;
        }

        // Move the contours into device-pixel space.
        let scaled: Vec<Vec<Point>> = self
            .contours
            .iter()
            .map(|contour| {
                contour
                    .iter()
                    .map(|&(x, y)| (x * scale, y * scale))
                    .collect()
            })
            .collect();

        let weight = 1.0 / SUBSAMPLES as f64;
        let mut coverage = vec![0f64; width as usize];
        let mut crossings: Vec<(f64, i32)> = Vec::new();

        for row in 0..height {
            coverage.iter_mut().for_each(|c| *c = 0.0);

            for sub in 0..SUBSAMPLES {
                let y = row as f64 + (sub as f64 + 0.5) * weight;
                crossings.clear();

                for contour in &scaled {
                    let count = contour.len();
                    if count < 2 {
                        continue;
                    }
                    for i in 0..count {
                        let (x0, y0) = contour[i];
                        let (x1, y1) = contour[(i + 1) % count];
                        if (y0 <= y) != (y1 <= y) {
                            let t = (y - y0) / (y1 - y0);
                            let x = x0 + t * (x1 - x0);
                            let direction = if y1 > y0 { 1 } else { -1 };
                            crossings.push((x, direction));
                        }
                    }
                }

                if crossings.is_empty() {
                    continue;
                }
                crossings
                    .sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

                let mut winding = 0i32;
                let mut crossings_seen = 0u32;
                let mut inside = false;
                let mut span_start = 0.0;
                for &(x, direction) in &crossings {
                    let was_inside = inside;
                    winding += direction;
                    crossings_seen += 1;
                    inside = match self.fill_rule {
                        FillRule::Nonzero => winding != 0,
                        FillRule::Evenodd => crossings_seen % 2 == 1,
                    };
                    if !was_inside && inside {
                        span_start = x;
                    } else if was_inside && !inside {
                        add_span(&mut coverage, span_start, x, weight);
                    }
                }
            }

            let row_start = (row as usize) * (width as usize) * 4;
            for (x, coverage) in coverage.iter().enumerate() {
                let alpha = (coverage * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
                let offset = row_start + x * 4;
                data[offset] = alpha;
                data[offset + 1] = alpha;
                data[offset + 2] = alpha;
                data[offset + 3] = alpha;
            }
        }

        data
    }
}

/// Accumulate the horizontal coverage of the span `[x0, x1)` into `coverage`,
/// weighing each sub-scanline by `weight`.
fn add_span(coverage: &mut [f64], x0: f64, x1: f64, weight: f64) {
    let limit = coverage.len() as f64;
    let x0 = x0.max(0.0);
    let x1 = x1.min(limit);
    if x1 <= x0 {
        return;
    }
    let first = x0.floor() as usize;
    let last = (x1.ceil() as usize).min(coverage.len());
    for (pixel, coverage) in coverage.iter_mut().enumerate().take(last).skip(first) {
        let left = (pixel as f64).max(x0);
        let right = ((pixel + 1) as f64).min(x1);
        if right > left {
            *coverage += (right - left) * weight;
        }
    }
}

/// A resolution-independent SVG-style path command, with all coordinates already
/// resolved to absolute CSS pixels.
#[derive(Clone, Copy, Debug)]
enum Cmd {
    Move(Point),
    Line(Point),
    Quad {
        control: Point,
        to: Point,
    },
    Cubic {
        control1: Point,
        control2: Point,
        to: Point,
    },
    Arc {
        radii: Point,
        rotation_deg: f64,
        large_arc: bool,
        sweep: bool,
        to: Point,
    },
    Close,
}

fn length_percentage_to_px(value: &LengthPercentage, basis: f64) -> f64 {
    value.resolve(Length::new(basis as f32)).px() as f64
}

fn resolve_position(position: &Position, size: Size2D<f64>) -> Point {
    (
        length_percentage_to_px(&position.horizontal, size.width),
        length_percentage_to_px(&position.vertical, size.height),
    )
}

fn path_endpoint(current: Point, endpoint: &CommandEndPoint<SVGPathPosition, f32>) -> Point {
    match endpoint {
        CommandEndPoint::ToPosition(position) => {
            (position.horizontal as f64, position.vertical as f64)
        },
        CommandEndPoint::ByCoordinate(coord) => {
            (current.0 + coord.x as f64, current.1 + coord.y as f64)
        },
    }
}

fn path_control(
    current: Point,
    endpoint: Point,
    control: &ControlPoint<SVGPathPosition, f32>,
) -> Point {
    match control {
        ControlPoint::Absolute(position) => (position.horizontal as f64, position.vertical as f64),
        ControlPoint::Relative(relative) => {
            let base = match relative.reference {
                ControlReference::Start => current,
                ControlReference::End => endpoint,
                ControlReference::Origin => (0.0, 0.0),
            };
            (
                base.0 + relative.coord.x as f64,
                base.1 + relative.coord.y as f64,
            )
        },
    }
}

fn shape_endpoint(
    current: Point,
    endpoint: &CommandEndPoint<Position, LengthPercentage>,
    size: Size2D<f64>,
) -> Point {
    match endpoint {
        CommandEndPoint::ToPosition(position) => resolve_position(position, size),
        CommandEndPoint::ByCoordinate(coord) => (
            current.0 + length_percentage_to_px(&coord.x, size.width),
            current.1 + length_percentage_to_px(&coord.y, size.height),
        ),
    }
}

fn shape_control(
    current: Point,
    endpoint: Point,
    control: &ControlPoint<Position, LengthPercentage>,
    size: Size2D<f64>,
) -> Point {
    match control {
        ControlPoint::Absolute(position) => resolve_position(position, size),
        ControlPoint::Relative(relative) => {
            let base = match relative.reference {
                ControlReference::Start => current,
                ControlReference::End => endpoint,
                ControlReference::Origin => (0.0, 0.0),
            };
            (
                base.0 + length_percentage_to_px(&relative.coord.x, size.width),
                base.1 + length_percentage_to_px(&relative.coord.y, size.height),
            )
        },
    }
}

/// Lower the SVG path commands of `path()` into absolute, resolution-independent
/// commands. `reference_size` is only needed for the (rare) keyword/percentage
/// forms that the SVG path grammar cannot actually produce, but it keeps the
/// lowering total.
fn lower_path_commands(commands: &[PathCommand], reference_size: Size2D<f64>) -> Vec<Cmd> {
    let mut result = Vec::with_capacity(commands.len());
    let mut current: Point = (0.0, 0.0);
    let mut last_cubic_control: Option<Point> = None;
    let mut last_quad_control: Option<Point> = None;

    for command in commands {
        match command {
            PathCommand::Move { point } => {
                let to = path_endpoint(current, point);
                result.push(Cmd::Move(to));
                current = to;
                last_cubic_control = None;
                last_quad_control = None;
            },
            PathCommand::Line { point } => {
                let to = path_endpoint(current, point);
                result.push(Cmd::Line(to));
                current = to;
                last_cubic_control = None;
                last_quad_control = None;
            },
            PathCommand::HLine { x } => {
                let new_x = match x {
                    AxisEndPoint::ToPosition(AxisPosition::LengthPercent(value)) => *value as f64,
                    AxisEndPoint::ToPosition(AxisPosition::Keyword(keyword)) => {
                        keyword.as_percentage().0 as f64 * reference_size.width
                    },
                    AxisEndPoint::ByCoordinate(value) => current.0 + *value as f64,
                };
                let to = (new_x, current.1);
                result.push(Cmd::Line(to));
                current = to;
                last_cubic_control = None;
                last_quad_control = None;
            },
            PathCommand::VLine { y } => {
                let new_y = match y {
                    AxisEndPoint::ToPosition(AxisPosition::LengthPercent(value)) => *value as f64,
                    AxisEndPoint::ToPosition(AxisPosition::Keyword(keyword)) => {
                        keyword.as_percentage().0 as f64 * reference_size.height
                    },
                    AxisEndPoint::ByCoordinate(value) => current.1 + *value as f64,
                };
                let to = (current.0, new_y);
                result.push(Cmd::Line(to));
                current = to;
                last_cubic_control = None;
                last_quad_control = None;
            },
            PathCommand::QuadCurve { point, control1 } => {
                let to = path_endpoint(current, point);
                let control = path_control(current, to, control1);
                result.push(Cmd::Quad { control, to });
                current = to;
                last_cubic_control = None;
                last_quad_control = Some(control);
            },
            PathCommand::CubicCurve {
                point,
                control1,
                control2,
            } => {
                let to = path_endpoint(current, point);
                let first = path_control(current, to, control1);
                let second = path_control(current, to, control2);
                result.push(Cmd::Cubic {
                    control1: first,
                    control2: second,
                    to,
                });
                current = to;
                last_cubic_control = Some(second);
                last_quad_control = None;
            },
            PathCommand::SmoothCubic { point, control2 } => {
                let to = path_endpoint(current, point);
                let first = match last_cubic_control {
                    Some(control) => reflect(current, control),
                    None => current,
                };
                let second = path_control(current, to, control2);
                result.push(Cmd::Cubic {
                    control1: first,
                    control2: second,
                    to,
                });
                current = to;
                last_cubic_control = Some(second);
                last_quad_control = None;
            },
            PathCommand::SmoothQuad { point } => {
                let to = path_endpoint(current, point);
                let control = match last_quad_control {
                    Some(control) => reflect(current, control),
                    None => current,
                };
                result.push(Cmd::Quad { control, to });
                current = to;
                last_cubic_control = None;
                last_quad_control = Some(control);
            },
            PathCommand::Arc {
                point,
                radii,
                arc_sweep,
                arc_size,
                rotate,
            } => {
                let to = path_endpoint(current, point);
                let rx = (radii.rx as f64).abs();
                let ry = match radii.ry {
                    Optional::Some(value) => (value as f64).abs(),
                    Optional::None => rx,
                };
                result.push(Cmd::Arc {
                    radii: (rx, ry),
                    rotation_deg: *rotate as f64,
                    large_arc: matches!(arc_size, ArcSize::Large),
                    sweep: matches!(arc_sweep, ArcSweep::Cw),
                    to,
                });
                current = to;
                last_cubic_control = None;
                last_quad_control = None;
            },
            PathCommand::Close => {
                result.push(Cmd::Close);
                last_cubic_control = None;
                last_quad_control = None;
            },
        }
    }

    result
}

/// Lower the `shape()` commands into absolute, resolution-independent commands,
/// resolving `to`/`by` positions, control points and arc radii against the
/// reference box described by `reference_size`.
fn lower_shape_commands(commands: &[ShapeCommand], reference_size: Size2D<f64>) -> Vec<Cmd> {
    let mut result = Vec::with_capacity(commands.len());
    let mut current: Point = (0.0, 0.0);
    let mut last_cubic_control: Option<Point> = None;
    let mut last_quad_control: Option<Point> = None;

    for command in commands {
        match command {
            ShapeCommand::Move { point } => {
                let to = shape_endpoint(current, point, reference_size);
                result.push(Cmd::Move(to));
                current = to;
                last_cubic_control = None;
                last_quad_control = None;
            },
            ShapeCommand::Line { point } => {
                let to = shape_endpoint(current, point, reference_size);
                result.push(Cmd::Line(to));
                current = to;
                last_cubic_control = None;
                last_quad_control = None;
            },
            ShapeCommand::HLine { x } => {
                let new_x = match x {
                    AxisEndPoint::ToPosition(AxisPosition::LengthPercent(value)) => {
                        length_percentage_to_px(value, reference_size.width)
                    },
                    AxisEndPoint::ToPosition(AxisPosition::Keyword(keyword)) => {
                        keyword.as_percentage().0 as f64 * reference_size.width
                    },
                    AxisEndPoint::ByCoordinate(value) => {
                        current.0 + length_percentage_to_px(value, reference_size.width)
                    },
                };
                let to = (new_x, current.1);
                result.push(Cmd::Line(to));
                current = to;
                last_cubic_control = None;
                last_quad_control = None;
            },
            ShapeCommand::VLine { y } => {
                let new_y = match y {
                    AxisEndPoint::ToPosition(AxisPosition::LengthPercent(value)) => {
                        length_percentage_to_px(value, reference_size.height)
                    },
                    AxisEndPoint::ToPosition(AxisPosition::Keyword(keyword)) => {
                        keyword.as_percentage().0 as f64 * reference_size.height
                    },
                    AxisEndPoint::ByCoordinate(value) => {
                        current.1 + length_percentage_to_px(value, reference_size.height)
                    },
                };
                let to = (current.0, new_y);
                result.push(Cmd::Line(to));
                current = to;
                last_cubic_control = None;
                last_quad_control = None;
            },
            ShapeCommand::QuadCurve { point, control1 } => {
                let to = shape_endpoint(current, point, reference_size);
                let control = shape_control(current, to, control1, reference_size);
                result.push(Cmd::Quad { control, to });
                current = to;
                last_cubic_control = None;
                last_quad_control = Some(control);
            },
            ShapeCommand::CubicCurve {
                point,
                control1,
                control2,
            } => {
                let to = shape_endpoint(current, point, reference_size);
                let first = shape_control(current, to, control1, reference_size);
                let second = shape_control(current, to, control2, reference_size);
                result.push(Cmd::Cubic {
                    control1: first,
                    control2: second,
                    to,
                });
                current = to;
                last_cubic_control = Some(second);
                last_quad_control = None;
            },
            ShapeCommand::SmoothCubic { point, control2 } => {
                let to = shape_endpoint(current, point, reference_size);
                let first = match last_cubic_control {
                    Some(control) => reflect(current, control),
                    None => current,
                };
                let second = shape_control(current, to, control2, reference_size);
                result.push(Cmd::Cubic {
                    control1: first,
                    control2: second,
                    to,
                });
                current = to;
                last_cubic_control = Some(second);
                last_quad_control = None;
            },
            ShapeCommand::SmoothQuad { point } => {
                let to = shape_endpoint(current, point, reference_size);
                let control = match last_quad_control {
                    Some(control) => reflect(current, control),
                    None => current,
                };
                result.push(Cmd::Quad { control, to });
                current = to;
                last_cubic_control = None;
                last_quad_control = Some(control);
            },
            ShapeCommand::Arc {
                point,
                radii,
                arc_sweep,
                arc_size,
                rotate,
            } => {
                let to = shape_endpoint(current, point, reference_size);
                // <arc-command> radii percentages resolve against the
                // direction-agnostic size of the reference box.
                let diagonal =
                    (reference_size.width.powi(2) + reference_size.height.powi(2)).sqrt() / SQRT_2;
                let rx = length_percentage_to_px(&radii.rx, diagonal).abs();
                let ry = match radii.ry {
                    Optional::Some(ref value) => length_percentage_to_px(value, diagonal).abs(),
                    Optional::None => rx,
                };
                result.push(Cmd::Arc {
                    radii: (rx, ry),
                    rotation_deg: rotate.degrees() as f64,
                    large_arc: matches!(arc_size, ArcSize::Large),
                    sweep: matches!(arc_sweep, ArcSweep::Cw),
                    to,
                });
                current = to;
                last_cubic_control = None;
                last_quad_control = None;
            },
            ShapeCommand::Close => {
                result.push(Cmd::Close);
                last_cubic_control = None;
                last_quad_control = None;
            },
        }
    }

    result
}

fn reflect(point: Point, control: Point) -> Point {
    (2.0 * point.0 - control.0, 2.0 * point.1 - control.1)
}

/// Flatten the absolute commands into a list of closed contours.
/// Build path commands for a polygon whose vertices are rounded with `radius`.
///
/// Each corner is replaced by a circular arc tangent to both adjacent edges.
/// As required by the specification, the radius is clamped so that a fillet
/// never consumes more than half of either adjacent edge.
///
/// <https://drafts.csswg.org/css-shapes-1/#funcdef-basic-shape-polygon>
fn rounded_polygon_commands(points: &[Point], radius: f64) -> Vec<Cmd> {
    let count = points.len();
    if count < 3 || radius <= 0.0 {
        return Vec::new();
    }

    let mut commands = Vec::with_capacity(count * 3 + 1);
    let mut started = false;

    for index in 0..count {
        let previous = points[(index + count - 1) % count];
        let current = points[index];
        let next = points[(index + 1) % count];

        let to_previous = (previous.0 - current.0, previous.1 - current.1);
        let to_next = (next.0 - current.0, next.1 - current.1);
        let previous_length =
            (to_previous.0 * to_previous.0 + to_previous.1 * to_previous.1).sqrt();
        let next_length = (to_next.0 * to_next.0 + to_next.1 * to_next.1).sqrt();

        let mut start = current;
        let mut end = current;
        let mut sweep = false;
        let mut fillet_radius = 0.0;

        if previous_length > 0.0 && next_length > 0.0 {
            let previous_direction = (
                to_previous.0 / previous_length,
                to_previous.1 / previous_length,
            );
            let next_direction = (to_next.0 / next_length, to_next.1 / next_length);
            let cosine = (previous_direction.0 * next_direction.0 +
                previous_direction.1 * next_direction.1)
                .clamp(-1.0, 1.0);
            let angle = cosine.acos();
            if angle > 1e-6 && (PI - angle) > 1e-6 {
                let half_angle_tangent = (angle / 2.0).tan();
                let tangent = (radius / half_angle_tangent)
                    .min(previous_length / 2.0)
                    .min(next_length / 2.0);
                fillet_radius = tangent * half_angle_tangent;
                start = (
                    current.0 + previous_direction.0 * tangent,
                    current.1 + previous_direction.1 * tangent,
                );
                end = (
                    current.0 + next_direction.0 * tangent,
                    current.1 + next_direction.1 * tangent,
                );
                sweep = previous_direction.0 * next_direction.1 -
                    previous_direction.1 * next_direction.0 <
                    0.0;
            }
        }

        if started {
            commands.push(Cmd::Line(start));
        } else {
            commands.push(Cmd::Move(start));
            started = true;
        }
        if fillet_radius > 0.0 {
            commands.push(Cmd::Arc {
                radii: (fillet_radius, fillet_radius),
                rotation_deg: 0.0,
                large_arc: false,
                sweep,
                to: end,
            });
        }
    }

    commands.push(Cmd::Close);
    commands
}

fn flatten_commands(commands: &[Cmd]) -> Vec<Vec<Point>> {
    let mut contours: Vec<Vec<Point>> = Vec::new();
    let mut contour: Vec<Point> = Vec::new();
    let mut current: Point = (0.0, 0.0);
    let mut subpath_start: Point = (0.0, 0.0);

    for command in commands {
        match command {
            Cmd::Move(to) => {
                push_contour(&mut contours, &mut contour);
                current = *to;
                subpath_start = *to;
                contour.push(*to);
            },
            Cmd::Line(to) => {
                if contour.is_empty() {
                    contour.push(current);
                }
                contour.push(*to);
                current = *to;
            },
            Cmd::Quad { control, to } => {
                if contour.is_empty() {
                    contour.push(current);
                }
                flatten_quad(current, *control, *to, 0, &mut contour);
                current = *to;
            },
            Cmd::Cubic {
                control1,
                control2,
                to,
            } => {
                if contour.is_empty() {
                    contour.push(current);
                }
                flatten_cubic(current, *control1, *control2, *to, 0, &mut contour);
                current = *to;
            },
            Cmd::Arc {
                radii,
                rotation_deg,
                large_arc,
                sweep,
                to,
            } => {
                if contour.is_empty() {
                    contour.push(current);
                }
                arc_to_cubics(
                    current,
                    *to,
                    *radii,
                    *rotation_deg,
                    *large_arc,
                    *sweep,
                    &mut contour,
                );
                current = *to;
            },
            Cmd::Close => {
                if !contour.is_empty() {
                    contour.push(subpath_start);
                    current = subpath_start;
                }
            },
        }
    }
    push_contour(&mut contours, &mut contour);
    contours
}

fn push_contour(contours: &mut Vec<Vec<Point>>, contour: &mut Vec<Point>) {
    if contour.len() > 2 {
        contours.push(std::mem::take(contour));
    } else {
        contour.clear();
    }
}

fn flatten_quad(p0: Point, p1: Point, p2: Point, depth: u32, out: &mut Vec<Point>) {
    if depth >= MAX_FLATTEN_DEPTH || distance_to_line(p1, p0, p2) <= FLATTEN_TOLERANCE {
        out.push(p2);
        return;
    }
    let p01 = midpoint(p0, p1);
    let p12 = midpoint(p1, p2);
    let p012 = midpoint(p01, p12);
    flatten_quad(p0, p01, p012, depth + 1, out);
    flatten_quad(p012, p12, p2, depth + 1, out);
}

fn flatten_cubic(p0: Point, p1: Point, p2: Point, p3: Point, depth: u32, out: &mut Vec<Point>) {
    if depth >= MAX_FLATTEN_DEPTH ||
        (distance_to_line(p1, p0, p3) <= FLATTEN_TOLERANCE &&
            distance_to_line(p2, p0, p3) <= FLATTEN_TOLERANCE)
    {
        out.push(p3);
        return;
    }
    let p01 = midpoint(p0, p1);
    let p12 = midpoint(p1, p2);
    let p23 = midpoint(p2, p3);
    let p012 = midpoint(p01, p12);
    let p123 = midpoint(p12, p23);
    let p0123 = midpoint(p012, p123);
    flatten_cubic(p0, p01, p012, p0123, depth + 1, out);
    flatten_cubic(p0123, p123, p23, p3, depth + 1, out);
}

fn midpoint(a: Point, b: Point) -> Point {
    ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0)
}

fn distance_to_line(point: Point, a: Point, b: Point) -> f64 {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let length_squared = dx * dx + dy * dy;
    if length_squared == 0.0 {
        return ((point.0 - a.0).powi(2) + (point.1 - a.1).powi(2)).sqrt();
    }
    let t = (((point.0 - a.0) * dx + (point.1 - a.1) * dy) / length_squared).clamp(0.0, 1.0);
    let projection = (a.0 + t * dx, a.1 + t * dy);
    ((point.0 - projection.0).powi(2) + (point.1 - projection.1).powi(2)).sqrt()
}

/// Approximate an SVG elliptical arc from `from` to `to` with cubic Bézier
/// segments, appending the flattened result to `out`. This is the endpoint to
/// center parameterization from SVG 1.1 appendix F.6.
#[allow(clippy::too_many_arguments)]
fn arc_to_cubics(
    from: Point,
    to: Point,
    radii: Point,
    rotation_deg: f64,
    large_arc: bool,
    sweep: bool,
    out: &mut Vec<Point>,
) {
    if from == to {
        return;
    }

    let (mut rx, mut ry) = (radii.0.abs(), radii.1.abs());
    if rx == 0.0 || ry == 0.0 {
        out.push(to);
        return;
    }

    let phi = rotation_deg.to_radians();
    let (sin_phi, cos_phi) = phi.sin_cos();

    let dx = (from.0 - to.0) / 2.0;
    let dy = (from.1 - to.1) / 2.0;
    let x1p = cos_phi * dx + sin_phi * dy;
    let y1p = -sin_phi * dx + cos_phi * dy;

    let lambda = (x1p * x1p) / (rx * rx) + (y1p * y1p) / (ry * ry);
    if lambda > 1.0 {
        let scale = lambda.sqrt();
        rx *= scale;
        ry *= scale;
    }

    let sign = if large_arc != sweep { 1.0 } else { -1.0 };
    let numerator = (rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p).max(0.0);
    let denominator = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    let coefficient = if denominator == 0.0 {
        0.0
    } else {
        sign * (numerator / denominator).sqrt()
    };
    let cxp = coefficient * rx * y1p / ry;
    let cyp = coefficient * -ry * x1p / rx;

    let center = (
        cos_phi * cxp - sin_phi * cyp + (from.0 + to.0) / 2.0,
        sin_phi * cxp + cos_phi * cyp + (from.1 + to.1) / 2.0,
    );

    let ux = (x1p - cxp) / rx;
    let uy = (y1p - cyp) / ry;
    let vx = (-x1p - cxp) / rx;
    let vy = (-y1p - cyp) / ry;

    let start_angle = uy.atan2(ux);
    let mut delta = angle_between(ux, uy, vx, vy);
    if !sweep && delta > 0.0 {
        delta -= 2.0 * PI;
    } else if sweep && delta < 0.0 {
        delta += 2.0 * PI;
    }

    let segments = (delta.abs() / FRAC_PI_2).ceil().max(1.0) as u32;
    let segment_angle = delta / segments as f64;

    let mut start = start_angle;
    for _ in 0..segments {
        let end = start + segment_angle;
        let alpha = 4.0 / 3.0 * ((end - start) / 4.0).tan();

        let (sin_start, cos_start) = start.sin_cos();
        let (sin_end, cos_end) = end.sin_cos();

        let p1 = ellipse_point(center, rx, ry, sin_phi, cos_phi, cos_start, sin_start);
        let p2 = ellipse_point(center, rx, ry, sin_phi, cos_phi, cos_end, sin_end);
        let d1 = ellipse_derivative(rx, ry, sin_phi, cos_phi, cos_start, sin_start);
        let d2 = ellipse_derivative(rx, ry, sin_phi, cos_phi, cos_end, sin_end);

        let control1 = (p1.0 + alpha * d1.0, p1.1 + alpha * d1.1);
        let control2 = (p2.0 - alpha * d2.0, p2.1 - alpha * d2.1);

        flatten_cubic(p1, control1, control2, p2, 0, out);
        start = end;
    }
}

fn ellipse_point(
    center: Point,
    rx: f64,
    ry: f64,
    sin_phi: f64,
    cos_phi: f64,
    cos_t: f64,
    sin_t: f64,
) -> Point {
    (
        center.0 + rx * cos_phi * cos_t - ry * sin_phi * sin_t,
        center.1 + rx * sin_phi * cos_t + ry * cos_phi * sin_t,
    )
}

fn ellipse_derivative(
    rx: f64,
    ry: f64,
    sin_phi: f64,
    cos_phi: f64,
    cos_t: f64,
    sin_t: f64,
) -> Point {
    (
        -rx * cos_phi * sin_t - ry * sin_phi * cos_t,
        -rx * sin_phi * sin_t + ry * cos_phi * cos_t,
    )
}

fn angle_between(ux: f64, uy: f64, vx: f64, vy: f64) -> f64 {
    let sign = if ux * vy - uy * vx >= 0.0 { 1.0 } else { -1.0 };
    let dot = (ux * vx + uy * vy).clamp(-1.0, 1.0);
    sign * dot.acos()
}
