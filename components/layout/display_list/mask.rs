/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Painting of `mask-image` for CSS boxes.
//!
//! Servo models `mask-image` as a list of images (SVG `<mask>` references are
//! not representable in the shared `Image` type). WebRender exposes an image
//! mask clip which samples the red channel of an R8 image as coverage. This
//! module rasterizes the mask layers into such an R8 image and attaches it to
//! the element's stacking context as an image-mask clip.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use app_units::Au;
use net_traits::image_cache::Image as CachedImage;
use paint_api::SerializableImageData;
use pixels::{PixelFormat, RasterImage};
use servo_base::generic_channel::GenericSharedMemory;
use style::color::ColorSpace;
use style::properties::ComputedValues;
use style::properties::longhands::mask_mode::single_value::computed_value::T as MaskMode;
use style::properties::longhands::mask_origin::single_value::computed_value::T as MaskOrigin;
use style::values::computed::image::{Gradient, LineDirection};
use style::values::computed::{BackgroundClip, BackgroundSize, Color, Image, LengthPercentage};
use style::values::generics::image::GradientItem;
use style::values::specified::background::BackgroundRepeatKeyword as Repeat;
use webrender_api::units::{DeviceIntSize, LayoutRect, LayoutSize};
use webrender_api::{
    ClipChainId, ImageDescriptor, ImageDescriptorFlags, ImageFormat, ImageMask, SpatialId,
};

use super::{BuilderForBoxFragment, DisplayListBuilder};
use crate::context::{ImageResolver, ResolvedImage};

/// An sRGB `(r, g, b, a)` color with components in the `0..=1` range.
type MaskColor = (f32, f32, f32, f32);

/// Returns whether the given style has any effective mask image.
pub(super) fn has_mask(style: &ComputedValues) -> bool {
    style
        .get_svg()
        .mask_image
        .0
        .iter()
        .any(|image| !matches!(image, Image::None))
}

/// Paint the mask layers for the given box into an R8 WebRender image and attach
/// it as an image-mask clip to `spatial_id`.
pub(super) fn create_mask_clip(
    builder: &mut DisplayListBuilder,
    fragment_builder: &BuilderForBoxFragment<'_>,
    spatial_id: SpatialId,
    parent_clip_chain_id: Option<ClipChainId>,
) -> Option<ClipChainId> {
    let style = fragment_builder.fragment.style();
    if !has_mask(style) {
        return None;
    }

    let surface_rect = fragment_builder.border_rect;
    if surface_rect.width() <= 0.0 || surface_rect.height() <= 0.0 {
        return None;
    }

    let device_pixel_ratio = builder.device_pixel_ratio.get();
    let width = ((surface_rect.width() * device_pixel_ratio).round() as i32).max(1);
    let height = ((surface_rect.height() * device_pixel_ratio).round() as i32).max(1);
    if width as u64 * height as u64 > 64 * 1024 * 1024 {
        return None;
    }

    let node = fragment_builder.fragment.base.tag.map(|tag| tag.node);
    let resolver = builder.image_resolver.clone();
    let svg = style.get_svg();
    let layer_count = svg.mask_image.0.len().max(1);

    let mut layers = Vec::with_capacity(layer_count);
    for index in 0..layer_count {
        let image = get_cyclic(&svg.mask_image.0, index);
        let mode = *get_cyclic(&svg.mask_mode.0, index);
        let position_x = get_cyclic(&svg.mask_position_x.0, index);
        let position_y = get_cyclic(&svg.mask_position_y.0, index);
        let size = get_cyclic(&svg.mask_size.0, index);
        let repeat = get_cyclic(&svg.mask_repeat.0, index);
        let origin = *get_cyclic(&svg.mask_origin.0, index);
        let clip = *get_cyclic(&svg.mask_clip.0, index);

        let Ok(resolved) = resolver.resolve_image(node, image) else {
            continue;
        };

        let origin_rect = box_rect(fragment_builder, origin.into());
        let clip_rect = box_rect(fragment_builder, clip.into());
        if origin_rect.size().is_empty() || clip_rect.size().is_empty() {
            continue;
        }

        let natural_size = match &resolved {
            ResolvedImage::Image { size, .. } => Some((size.width, size.height)),
            _ => None,
        };
        let tile_size = resolve_tile_size(size, origin_rect.size(), natural_size);
        if tile_size.0 <= 0.0 || tile_size.1 <= 0.0 {
            continue;
        }

        let position = (
            resolve_position(position_x, origin_rect.size().width - tile_size.0),
            resolve_position(position_y, origin_rect.size().height - tile_size.1),
        );

        let repeat_x = repeat.0;
        let repeat_y = repeat.1;
        let Some(source) = MaskSource::from_resolved(&resolver, &resolved, node) else {
            continue;
        };

        layers.push(MaskLayer {
            source,
            mode,
            origin_rect,
            clip_rect,
            position,
            tile_size,
            repeat_x,
            repeat_y,
        });
    }

    let pixels = rasterize(
        &layers,
        surface_rect,
        width as usize,
        height as usize,
        device_pixel_ratio,
        style,
    )?;

    let mut hasher = DefaultHasher::new();
    width.hash(&mut hasher);
    height.hash(&mut hasher);
    pixels.hash(&mut hasher);
    let cache_key = hasher.finish();

    let image_key = {
        let mut cache = resolver.mask_image_cache.lock();
        if let Some(key) = cache.get(&cache_key) {
            *key
        } else {
            let key = resolver
                .paint_api
                .lock()
                .generate_image_key_blocking(resolver.webview_id)?;
            let descriptor = ImageDescriptor {
                format: ImageFormat::R8,
                size: DeviceIntSize::new(width, height),
                stride: None,
                offset: 0,
                flags: ImageDescriptorFlags::empty(),
            };
            resolver.paint_api.lock().add_image(
                key,
                descriptor,
                SerializableImageData::Raw(GenericSharedMemory::from_vec(pixels)),
                false,
            );
            cache.insert(cache_key, key);
            key
        }
    };

    let image_mask = ImageMask {
        image: image_key,
        rect: surface_rect,
    };
    let clip_id = builder.wr().define_clip_image_mask(
        spatial_id,
        image_mask,
        &[],
        webrender_api::FillRule::Nonzero,
    );
    Some(
        builder
            .wr()
            .define_clip_chain(parent_clip_chain_id, [clip_id]),
    )
}

fn get_cyclic<T>(values: &[T], index: usize) -> &T {
    &values[index % values.len()]
}

#[derive(Clone, Copy)]
enum MaskBox {
    Content,
    Padding,
    Border,
}

impl From<MaskOrigin> for MaskBox {
    fn from(origin: MaskOrigin) -> Self {
        match origin {
            MaskOrigin::ContentBox => MaskBox::Content,
            MaskOrigin::PaddingBox => MaskBox::Padding,
            MaskOrigin::BorderBox => MaskBox::Border,
        }
    }
}

impl From<BackgroundClip> for MaskBox {
    fn from(clip: BackgroundClip) -> Self {
        match clip {
            BackgroundClip::ContentBox => MaskBox::Content,
            BackgroundClip::PaddingBox => MaskBox::Padding,
            BackgroundClip::BorderBox | BackgroundClip::BorderArea => MaskBox::Border,
        }
    }
}

fn box_rect(fragment_builder: &BuilderForBoxFragment<'_>, mask_box: MaskBox) -> LayoutRect {
    match mask_box {
        MaskBox::Content => *fragment_builder.content_rect(),
        MaskBox::Padding => *fragment_builder.padding_rect(),
        MaskBox::Border => fragment_builder.border_rect,
    }
}

fn resolve_position(position: &LengthPercentage, free_space: f32) -> f32 {
    position
        .to_used_value(Au::from_f32_px(free_space))
        .to_f32_px()
}

fn resolve_tile_size(
    size: &BackgroundSize,
    origin_size: LayoutSize,
    natural_size: Option<(f32, f32)>,
) -> (f32, f32) {
    let natural_ratio = natural_size.and_then(|(w, h)| (h != 0.0).then_some(w / h));
    let contain_or_cover = |cover: bool| {
        let mut tile_size = (origin_size.width, origin_size.height);
        if let Some(natural_ratio) = natural_ratio {
            let origin_ratio = if origin_size.height != 0.0 {
                origin_size.width / origin_size.height
            } else {
                1.0
            };
            let fit_width = if cover {
                origin_ratio > natural_ratio
            } else {
                origin_ratio <= natural_ratio
            };
            if fit_width {
                tile_size.1 = tile_size.0 / natural_ratio;
            } else {
                tile_size.0 = tile_size.1 * natural_ratio;
            }
        }
        tile_size
    };

    match size {
        BackgroundSize::Contain => contain_or_cover(false),
        BackgroundSize::Cover => contain_or_cover(true),
        BackgroundSize::ExplicitSize { width, height } => {
            let width = width.non_auto().map(|lp| {
                lp.to_used_value(Au::from_f32_px(origin_size.width))
                    .to_f32_px()
            });
            let height = height.non_auto().map(|lp| {
                lp.to_used_value(Au::from_f32_px(origin_size.height))
                    .to_f32_px()
            });

            if width.is_none() && height.is_none() {
                if let Some((w, h)) = natural_size {
                    return (w, h);
                }
                return (origin_size.width, origin_size.height);
            }

            match (width, height) {
                (Some(w), Some(h)) => (w, h),
                (Some(w), None) => {
                    let h = natural_ratio.map_or(origin_size.height, |ratio| w / ratio);
                    (w, h)
                },
                (None, Some(h)) => {
                    let w = natural_ratio.map_or(origin_size.width, |ratio| h * ratio);
                    (w, h)
                },
                (None, None) => (origin_size.width, origin_size.height),
            }
        },
    }
}

enum MaskSource<'a> {
    Image {
        pixels: MaskPixels,
        natural_size: (f32, f32),
    },
    Color(Color),
    Gradient(&'a Gradient),
}

impl<'a> MaskSource<'a> {
    fn from_resolved<'b>(
        resolver: &ImageResolver,
        resolved: &ResolvedImage<'b>,
        node: Option<style::dom::OpaqueNode>,
    ) -> Option<MaskSource<'b>> {
        match resolved {
            ResolvedImage::Image { image, size } => {
                let pixels = match image {
                    CachedImage::Raster(raster) => MaskPixels::Raster(raster.clone()),
                    CachedImage::Vector(vector) => {
                        let node = node?;
                        let size = DeviceIntSize::new(
                            (size.width.max(1.0)) as i32,
                            (size.height.max(1.0)) as i32,
                        );
                        let raster = resolver.rasterize_vector_image(
                            vector.id,
                            size,
                            node,
                            vector.svg_id,
                        )?;
                        MaskPixels::Owned(raster)
                    },
                };
                Some(MaskSource::Image {
                    pixels,
                    natural_size: (size.width, size.height),
                })
            },
            ResolvedImage::Color(color) => Some(MaskSource::Color((**color).clone())),
            ResolvedImage::Gradient(gradient) => Some(MaskSource::Gradient(gradient)),
        }
    }
}

enum MaskPixels {
    Raster(Arc<RasterImage>),
    Owned(RasterImage),
}

impl MaskPixels {
    fn as_raster_image(&self) -> &RasterImage {
        match self {
            MaskPixels::Raster(raster) => raster,
            MaskPixels::Owned(raster) => raster,
        }
    }
}

struct MaskLayer<'a> {
    source: MaskSource<'a>,
    mode: MaskMode,
    origin_rect: LayoutRect,
    clip_rect: LayoutRect,
    position: (f32, f32),
    tile_size: (f32, f32),
    repeat_x: Repeat,
    repeat_y: Repeat,
}

fn rasterize(
    layers: &[MaskLayer<'_>],
    surface_rect: LayoutRect,
    width: usize,
    height: usize,
    device_pixel_ratio: f32,
    style: &ComputedValues,
) -> Option<Vec<u8>> {
    let mut output = vec![0u8; width * height];
    let inv_dpr = 1.0 / device_pixel_ratio;

    for layer in layers {
        let tile_origin = (
            layer.origin_rect.min.x + layer.position.0,
            layer.origin_rect.min.y + layer.position.1,
        );
        let clip = layer.clip_rect;

        for y in 0..height {
            let layout_y = surface_rect.min.y + (y as f32 + 0.5) * inv_dpr;
            if layout_y < clip.min.y || layout_y >= clip.max.y {
                continue;
            }
            for x in 0..width {
                let layout_x = surface_rect.min.x + (x as f32 + 0.5) * inv_dpr;
                if layout_x < clip.min.x || layout_x >= clip.max.x {
                    continue;
                }

                let mut tile_x = layout_x - tile_origin.0;
                let mut tile_y = layout_y - tile_origin.1;
                if !apply_repeat(
                    &mut tile_x,
                    layer.tile_size.0,
                    layer.origin_rect.size().width,
                    layer.repeat_x,
                ) || !apply_repeat(
                    &mut tile_y,
                    layer.tile_size.1,
                    layer.origin_rect.size().height,
                    layer.repeat_y,
                ) {
                    continue;
                }

                let Some(coverage) = layer_coverage(layer, tile_x, tile_y, style) else {
                    continue;
                };
                if coverage <= 0.0 {
                    continue;
                }

                let index = y * width + x;
                let dst = output[index] as f32 / 255.0;
                let added = coverage + dst * (1.0 - coverage);
                output[index] = (added.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        }
    }

    Some(output)
}

fn apply_repeat(value: &mut f32, tile_size: f32, area: f32, repeat: Repeat) -> bool {
    if tile_size <= 0.0 || area <= 0.0 {
        return false;
    }
    match repeat {
        Repeat::NoRepeat => *value >= 0.0 && *value < tile_size,
        Repeat::Repeat => {
            *value = value.rem_euclid(tile_size);
            true
        },
        Repeat::Space => {
            let count = (area / tile_size).floor();
            if count <= 1.0 {
                return *value >= 0.0 && *value < tile_size;
            }
            if *value < 0.0 || *value >= area {
                return false;
            }
            let spacing = (area - count * tile_size) / (count - 1.0);
            let period = tile_size + spacing;
            let index = (*value / period).floor();
            let local = *value - index * period;
            if local < tile_size {
                *value = local;
                true
            } else {
                false
            }
        },
        Repeat::Round => {
            let count = (area / tile_size).round().max(1.0);
            let scaled_tile = area / count;
            *value = value.rem_euclid(scaled_tile) * (tile_size / scaled_tile);
            true
        },
    }
}

fn layer_coverage(
    layer: &MaskLayer<'_>,
    tile_x: f32,
    tile_y: f32,
    style: &ComputedValues,
) -> Option<f32> {
    let (red, green, blue, alpha) = match &layer.source {
        MaskSource::Gradient(gradient) => {
            gradient_color(gradient, tile_x, tile_y, layer.tile_size, style)?
        },
        MaskSource::Color(color) => {
            let color = style.resolve_color(color).to_color_space(ColorSpace::Srgb);
            (
                color.components.0,
                color.components.1,
                color.components.2,
                color.alpha,
            )
        },
        MaskSource::Image {
            pixels,
            natural_size,
        } => {
            let raster = pixels.as_raster_image();
            let (u, v) = (
                (tile_x / layer.tile_size.0).clamp(0.0, 0.999_999),
                (tile_y / layer.tile_size.1).clamp(0.0, 0.999_999),
            );
            let _ = natural_size;
            sample_image(raster, u, v)?
        },
    };

    let alpha = alpha.clamp(0.0, 1.0);
    match layer.mode {
        MaskMode::MatchSource | MaskMode::Alpha => Some(alpha),
        MaskMode::Luminance => {
            let luminance = 0.2126 * red.clamp(0.0, 1.0) +
                0.7152 * green.clamp(0.0, 1.0) +
                0.0722 * blue.clamp(0.0, 1.0);
            Some(luminance * alpha)
        },
    }
}

fn sample_image(raster: &RasterImage, u: f32, v: f32) -> Option<MaskColor> {
    let width = raster.metadata.width as usize;
    let height = raster.metadata.height as usize;
    if width == 0 || height == 0 {
        return None;
    }
    let frame_bytes = raster
        .frames
        .first()
        .map(|frame| &raster.bytes[frame.byte_range.clone()])
        .unwrap_or(&raster.bytes);

    let x = (u * width as f32) as usize;
    let y = (v * height as f32) as usize;
    let (bytes_per_pixel, alpha_index, red_index, green_index, blue_index, is_luminance) =
        match raster.format {
            PixelFormat::K8 => (1, 0, 0, 0, 0, true),
            PixelFormat::KA8 => (2, 1, 0, 0, 0, true),
            PixelFormat::RGB8 => (3, 3, 0, 1, 2, false),
            PixelFormat::RGBA8 => (4, 3, 0, 1, 2, false),
            PixelFormat::BGRA8 => (4, 3, 2, 1, 0, false),
        };

    let offset = (y * width + x) * bytes_per_pixel;
    if offset + bytes_per_pixel > frame_bytes.len() {
        return None;
    }
    let pixel = &frame_bytes[offset..offset + bytes_per_pixel];
    let alpha = if alpha_index < bytes_per_pixel {
        pixel[alpha_index] as f32 / 255.0
    } else {
        1.0
    };
    if is_luminance {
        let luminance = pixel[0] as f32 / 255.0;
        return Some((luminance, luminance, luminance, alpha));
    }
    Some((
        pixel[red_index] as f32 / 255.0,
        pixel[green_index] as f32 / 255.0,
        pixel[blue_index] as f32 / 255.0,
        alpha,
    ))
}

fn gradient_color(
    gradient: &Gradient,
    x: f32,
    y: f32,
    size: (f32, f32),
    style: &ComputedValues,
) -> Option<MaskColor> {
    let Gradient::Linear {
        items,
        direction,
        color_interpolation_method: _,
        flags,
        compat_mode: _,
    } = gradient
    else {
        return None;
    };

    let direction = match direction {
        LineDirection::Horizontal(
            style::values::specified::position::HorizontalPositionKeyword::Right,
        ) => (1.0, 0.0),
        LineDirection::Horizontal(
            style::values::specified::position::HorizontalPositionKeyword::Left,
        ) => (-1.0, 0.0),
        LineDirection::Vertical(
            style::values::specified::position::VerticalPositionKeyword::Bottom,
        ) => (0.0, 1.0),
        LineDirection::Vertical(
            style::values::specified::position::VerticalPositionKeyword::Top,
        ) => (0.0, -1.0),
        LineDirection::Angle(angle) => {
            let radians = angle.radians();
            (radians.sin(), -radians.cos())
        },
        LineDirection::Corner(horizontal, vertical) => {
            use style::values::specified::position::{
                HorizontalPositionKeyword as H, VerticalPositionKeyword as V,
            };
            let dx = if *horizontal == H::Right {
                size.1
            } else {
                -size.1
            };
            let dy = if *vertical == V::Bottom {
                size.0
            } else {
                -size.0
            };
            let length = (dx * dx + dy * dy).sqrt();
            if length == 0.0 {
                (1.0, 0.0)
            } else {
                (dx / length, dy / length)
            }
        },
    };

    let line_length = (size.0 * direction.0).abs() + (size.1 * direction.1).abs();
    if line_length == 0.0 {
        return None;
    }

    let center = (size.0 / 2.0, size.1 / 2.0);
    let start = (
        center.0 - direction.0 * line_length / 2.0,
        center.1 - direction.1 * line_length / 2.0,
    );
    let t = ((x - start.0) * direction.0 + (y - start.1) * direction.1) / line_length;
    let t = if flags.contains(style::values::generics::image::GradientFlags::REPEATING) {
        t.rem_euclid(1.0)
    } else {
        t.clamp(0.0, 1.0)
    };

    let stops = linear_color_stops(style, items, line_length);
    if stops.is_empty() {
        return None;
    }
    if stops.len() == 1 {
        return Some(stops[0].1);
    }
    if t <= stops[0].0 {
        return Some(stops[0].1);
    }
    for window in stops.windows(2) {
        let (p0, c0) = window[0];
        let (p1, c1) = window[1];
        if t <= p1 {
            let f = if p1 > p0 { (t - p0) / (p1 - p0) } else { 0.0 };
            return Some((
                c0.0 + (c1.0 - c0.0) * f,
                c0.1 + (c1.1 - c0.1) * f,
                c0.2 + (c1.2 - c0.2) * f,
                c0.3 + (c1.3 - c0.3) * f,
            ));
        }
    }
    Some(stops[stops.len() - 1].1)
}

fn linear_color_stops(
    style: &ComputedValues,
    items: &[GradientItem<Color, LengthPercentage>],
    line_length: f32,
) -> Vec<(f32, MaskColor)> {
    let mut stops: Vec<(Option<f32>, MaskColor)> = items
        .iter()
        .filter_map(|item| match item {
            GradientItem::SimpleColorStop(color) => Some((None, resolved_color(style, color))),
            GradientItem::ComplexColorStop { color, position } => Some((
                Some(
                    position
                        .to_used_value(Au::from_f32_px(line_length))
                        .to_f32_px() /
                        line_length,
                ),
                resolved_color(style, color),
            )),
            GradientItem::InterpolationHint(_) => None,
        })
        .collect();

    if stops.is_empty() {
        return vec![];
    }
    if stops[0].0.is_none() {
        stops[0].0 = Some(0.0);
    }
    let last = stops.len() - 1;
    if stops[last].0.is_none() {
        stops[last].0 = Some(1.0);
    }

    let mut max_position = stops[0].0.unwrap();
    for stop in stops.iter_mut() {
        if let Some(position) = stop.0 {
            if position < max_position {
                stop.0 = Some(max_position);
            } else {
                max_position = position;
            }
        }
    }

    // Give every stop a position, distributing unpositioned stops evenly
    // between their neighbours, as required by color stop fixup.
    let mut index = 0;
    while index < stops.len() {
        if stops[index].0.is_some() {
            index += 1;
            continue;
        }
        let previous = index - 1;
        let mut next = index;
        while stops[next].0.is_none() {
            next += 1;
        }
        let start = stops[previous].0.unwrap();
        let end = stops[next].0.unwrap();
        let step = (end - start) / (next - previous) as f32;
        for offset in 1..(next - previous) {
            stops[previous + offset].0 = Some(start + step * offset as f32);
        }
        index = next + 1;
    }

    stops
        .into_iter()
        .filter_map(|(position, color)| position.map(|position| (position, color)))
        .collect()
}

fn resolved_color(style: &ComputedValues, color: &Color) -> MaskColor {
    let color = style.resolve_color(color).to_color_space(ColorSpace::Srgb);
    (
        color.components.0.clamp(0.0, 1.0),
        color.components.1.clamp(0.0, 1.0),
        color.components.2.clamp(0.0, 1.0),
        color.alpha.clamp(0.0, 1.0),
    )
}
