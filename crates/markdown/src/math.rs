use collections::{HashMap, HashSet};
use gpui::{
    App, Bounds, Corners, DevicePixels, Hsla, InlineBox, Pixels, RenderImage, Rgba, SharedString,
    Size, SvgSize, Window, size,
};
use math_render::{MathMode, TypesetMath};
use parking_lot::Mutex;
use std::{collections::BTreeMap, ops::Range, sync::Arc};
use util::ResultExt as _;

use crate::parser::{CodeBlockKind, MarkdownEvent, MarkdownTag, MarkdownTagEnd};

/// The size of math relative to the text around it. The fonts math is drawn with have a
/// smaller x-height than most text fonts, so KaTeX, which shares them, scales them up by
/// about this much too.
const MATH_FONT_SCALE: f32 = 1.2;
/// The same formula can be on screen in several sizes and colors, say in a heading and
/// in a link.
const MAX_RASTERS_PER_EXPRESSION: usize = 12;
/// Rasters are drawn at a higher resolution to smooth their edges, unless that would
/// make them larger than this.
const MAX_SUPERSAMPLED_PIXELS: f32 = 4_000_000.;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MathKey {
    tex: SharedString,
    mode: MathMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RasterKey {
    size: Size<DevicePixels>,
    color: [u8; 4],
}

/// A formula along with the images drawn of it so far.
#[derive(Debug)]
pub(crate) struct MathExpression {
    typeset: Result<TypesetMath, SharedString>,
    /// Ordered from least to most recently used. A missing image marks a failed raster.
    rasters: Mutex<Vec<(RasterKey, Option<Arc<RenderImage>>)>>,
}

#[derive(Clone, Debug)]
pub(crate) struct ParsedMath {
    key: MathKey,
    pub(crate) expression: Arc<MathExpression>,
}

impl ParsedMath {
    pub(crate) fn tex(&self) -> &SharedString {
        &self.key.tex
    }
}

/// The math in a document, keyed by source offset.
pub(crate) type MathExpressions = BTreeMap<usize, ParsedMath>;

/// Typesets the math in a document, reusing the expressions of `previous` where the
/// formula hasn't changed. This blocks, so it should be called from a background thread.
pub(crate) fn typeset_math(
    source: &str,
    events: &[(Range<usize>, MarkdownEvent)],
    previous: &MathExpressions,
) -> MathExpressions {
    let mut expressions = previous
        .values()
        .map(|math| (math.key.clone(), math.expression.clone()))
        .collect::<HashMap<_, _>>();
    let mut parsed_math = MathExpressions::new();
    let mut add = |source_offset: usize, tex: &str, mode: MathMode| {
        let key = MathKey {
            tex: SharedString::from(tex.trim().to_string()),
            mode,
        };
        let expression = expressions
            .entry(key.clone())
            .or_insert_with(|| Arc::new(MathExpression::new(&key)))
            .clone();
        parsed_math.insert(source_offset, ParsedMath { key, expression });
    };

    // The source offset and text of the math code block being read.
    let mut code_block: Option<(usize, String)> = None;
    for (range, event) in events {
        match event {
            MarkdownEvent::InlineMath(tex) => add(range.start, tex, MathMode::Inline),
            MarkdownEvent::DisplayMath(tex) => add(range.start, tex, MathMode::Display),
            MarkdownEvent::Start(MarkdownTag::CodeBlock {
                kind: CodeBlockKind::FencedLang(language),
                metadata,
            }) if language.as_ref() == "math" && metadata.is_fenced_closed => {
                code_block = Some((range.start, String::new()));
            }
            MarkdownEvent::Text => {
                if let Some((_, tex)) = &mut code_block {
                    tex.push_str(&source[range.clone()]);
                }
            }
            MarkdownEvent::SubstitutedText(text) => {
                if let Some((_, tex)) = &mut code_block {
                    tex.push_str(text);
                }
            }
            MarkdownEvent::End(MarkdownTagEnd::CodeBlock) => {
                if let Some((source_offset, tex)) = code_block.take() {
                    add(source_offset, &tex, MathMode::Display);
                }
            }
            _ => {}
        }
    }
    parsed_math
}

impl MathExpression {
    fn new(key: &MathKey) -> Self {
        Self {
            typeset: math_render::typeset(&key.tex, key.mode)
                .map_err(|error| SharedString::from(error.to_string())),
            rasters: Mutex::default(),
        }
    }

    /// Why the formula couldn't be typeset, if it couldn't.
    pub(crate) fn error(&self) -> Option<&SharedString> {
        self.typeset.as_ref().err()
    }

    /// The space for this formula among text of the given size and color, drawn in place
    /// of the text in `range`.
    pub(crate) fn inline_box(
        self: &Arc<Self>,
        range: Range<usize>,
        font_size: Pixels,
        color: Hsla,
    ) -> Option<InlineBox> {
        let typeset = self.typeset.as_ref().ok()?;
        let em = font_size * MATH_FONT_SCALE;
        let expression = self.clone();
        Some(InlineBox::new(
            range,
            em * typeset.width(),
            em * typeset.ascent(),
            em * typeset.descent(),
            move |bounds, window, cx| expression.paint(bounds, color, window, cx),
        ))
    }

    fn paint(&self, bounds: Bounds<Pixels>, color: Hsla, window: &mut Window, cx: &mut App) {
        let Ok(typeset) = &self.typeset else {
            return;
        };
        let em = bounds.size.width / typeset.width();
        let image_bounds = bounds.dilate(em * typeset.padding());
        // Drawing is left until a formula is visible, so that a long document doesn't
        // draw all of its formulas at once.
        if !image_bounds.intersects(&window.content_mask().bounds) {
            return;
        }

        let device_width = f32::from(image_bounds.size.width) * window.scale_factor();
        let device_height = f32::from(image_bounds.size.height) * window.scale_factor();
        let device_area = device_width * device_height;
        let supersampling = if device_area * 4. <= MAX_SUPERSAMPLED_PIXELS {
            2.
        } else {
            1.
        };
        let color = Rgba::from(color);
        let to_byte = |channel: f32| (channel.clamp(0., 1.) * 255.).round() as u8;
        let key = RasterKey {
            size: size(
                DevicePixels((device_width * supersampling).ceil() as i32),
                DevicePixels((device_height * supersampling).ceil() as i32),
            ),
            color: [color.r, color.g, color.b, color.a].map(to_byte),
        };

        if let Some(image) = self.raster(typeset, key, window, cx) {
            window
                .paint_image(
                    image_bounds,
                    image_bounds,
                    Corners::default(),
                    image,
                    0,
                    false,
                )
                .log_err();
        }
    }

    fn raster(
        &self,
        typeset: &TypesetMath,
        key: RasterKey,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Arc<RenderImage>> {
        let mut rasters = self.rasters.lock();
        if let Some(index) = rasters
            .iter()
            .position(|(raster_key, _)| *raster_key == key)
        {
            let raster = rasters.remove(index);
            let image = raster.1.clone();
            rasters.push(raster);
            return image;
        }

        let svg_renderer = cx.svg_renderer();
        let image = svg_renderer
            .parse_svg(typeset.svg(key.color).as_bytes())
            .and_then(|tree| svg_renderer.render_parsed(&tree, SvgSize::ExactSize(key.size)))
            .log_err();
        rasters.push((key, image.clone()));

        if rasters.len() > MAX_RASTERS_PER_EXPRESSION
            && let (_, Some(evicted)) = rasters.remove(0)
        {
            // The frame being painted may still refer to the evicted image.
            window.on_next_frame(move |window, cx| cx.drop_image(evicted, Some(window)));
        }
        image
    }

    fn drop_images(&self, cx: &mut App) {
        for (_, image) in self.rasters.lock().drain(..) {
            if let Some(image) = image {
                cx.drop_image(image, None);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn raster_colors(&self) -> Vec<[u8; 4]> {
        self.rasters
            .lock()
            .iter()
            .filter(|(_, image)| image.is_some())
            .map(|(key, _)| key.color)
            .collect()
    }
}

/// Frees the images of the expressions in `old` that `new` no longer uses.
pub(crate) fn drop_unused_images(old: &MathExpressions, new: &MathExpressions, cx: &mut App) {
    let still_used = new
        .values()
        .map(|math| Arc::as_ptr(&math.expression))
        .collect::<HashSet<_>>();
    for math in old.values() {
        if !still_used.contains(&Arc::as_ptr(&math.expression)) {
            math.expression.drop_images(cx);
        }
    }
}
