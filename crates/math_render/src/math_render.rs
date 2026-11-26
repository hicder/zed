//! Typesets LaTeX math into self-contained SVG documents.

use anyhow::{Context as _, Result, anyhow};
use ratex_layout::{LayoutOptions, layout, to_display_list};
use ratex_svg::{SvgColorSyntax, SvgOptions, render_to_svg_with_color_syntax};
use ratex_types::{color::Color, math_style::MathStyle};

/// SVG user units per em.
const UNITS_PER_EM: f32 = 100.;
/// Space around the typeset box, in ems, for ink such as italic overhangs that extends
/// past the box a formula is laid out in.
const PADDING_EM: f32 = 0.15;

/// The typesetter recurses once per level of nesting, and its own limit on nesting is
/// reached only after the stacks of background threads, which can be as small as 512 KiB,
/// have overflowed.
const TYPESETTING_STACK_SIZE: usize = 8 * 1024 * 1024;

const MAX_TEX_BYTES: usize = 20_000;
// Commands such as `\rule` can ask for output of any size, which must not reach the
// rasterizer.
const MAX_DIMENSION_EM: f64 = 500.;
const MAX_AREA_EM: f64 = 4_000.;

/// Stands for the color of the surrounding text, which isn't known while typesetting.
const TEXT_COLOR: [u8; 3] = [1, 2, 3];
const TEXT_COLOR_SVG: &str = "rgb(1,2,3)";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MathMode {
    /// Compact layout for math within a line of text, like `$...$`.
    Inline,
    /// Layout for math on a line of its own, like `$$...$$`.
    Display,
}

/// A typeset formula. All of its dimensions are in ems.
#[derive(Debug)]
pub struct TypesetMath {
    width: f32,
    ascent: f32,
    descent: f32,
    svg: String,
}

impl TypesetMath {
    pub fn width(&self) -> f32 {
        self.width
    }

    /// The height of the formula above the baseline.
    pub fn ascent(&self) -> f32 {
        self.ascent
    }

    /// The depth of the formula below the baseline.
    pub fn descent(&self) -> f32 {
        self.descent
    }

    /// How far the SVG document extends past the formula on every side.
    pub fn padding(&self) -> f32 {
        PADDING_EM
    }

    /// An SVG document drawing the formula in `color`, given as RGBA. Colors set by the
    /// formula itself, as with `\color`, are kept. Glyphs are drawn as paths, so the
    /// document doesn't depend on any fonts.
    pub fn svg(&self, color: [u8; 4]) -> String {
        let [red, green, blue, alpha] = color;
        let svg = self
            .svg
            .replace(TEXT_COLOR_SVG, &format!("rgb({red},{green},{blue})"));
        if alpha == u8::MAX {
            return svg;
        }
        let Some((opening_tag, body)) = svg.split_once('>') else {
            return svg;
        };
        let body = body.strip_suffix("</svg>").unwrap_or(body);
        let opacity = f32::from(alpha) / f32::from(u8::MAX);
        format!(r#"{opening_tag}><g opacity="{opacity}">{body}</g></svg>"#)
    }
}

/// Typesets `tex`, which must not include delimiters such as `$`.
///
/// This blocks while it works, so it should be called from a background thread.
pub fn typeset(tex: &str, mode: MathMode) -> Result<TypesetMath> {
    anyhow::ensure!(
        tex.len() <= MAX_TEX_BYTES,
        "formula is longer than {MAX_TEX_BYTES} bytes"
    );
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("math-typesetting".into())
            .stack_size(TYPESETTING_STACK_SIZE)
            .spawn_scoped(scope, || typeset_on_current_thread(tex, mode))
            .context("failed to start the typesetting thread")?
            .join()
            .map_err(|_| anyhow!("the typesetter panicked"))?
    })
}

fn typeset_on_current_thread(tex: &str, mode: MathMode) -> Result<TypesetMath> {
    let nodes = ratex_parser::parser::parse(tex).map_err(|error| anyhow!("{error}"))?;

    let [red, green, blue] = TEXT_COLOR.map(|channel| f32::from(channel) / f32::from(u8::MAX));
    let mut options = LayoutOptions::default();
    options.style = match mode {
        MathMode::Inline => MathStyle::Text,
        MathMode::Display => MathStyle::Display,
    };
    options.color = Color::new(red, green, blue, 1.);
    let display_list = to_display_list(&layout(&nodes, &options));

    let width = display_list.width;
    let height = display_list.height + display_list.depth;
    anyhow::ensure!(width > 0. && height > 0., "formula is empty");
    anyhow::ensure!(
        width <= MAX_DIMENSION_EM && height <= MAX_DIMENSION_EM && width * height <= MAX_AREA_EM,
        "formula is too large to draw"
    );

    let svg = render_to_svg_with_color_syntax(
        &display_list,
        &SvgOptions {
            font_size: f64::from(UNITS_PER_EM),
            padding: f64::from(PADDING_EM * UNITS_PER_EM),
            stroke_width: f64::from(0.04 * UNITS_PER_EM),
            embed_glyphs: true,
            font_dir: String::new(),
        },
        SvgColorSyntax::Rgb,
    );

    Ok(TypesetMath {
        width: width as f32,
        ascent: display_list.height as f32,
        descent: display_list.depth as f32,
        svg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_svg(svg: &str) -> usvg::Tree {
        usvg::Tree::from_str(svg, &usvg::Options::default()).expect("svg should parse")
    }

    #[test]
    fn typesets_common_constructs_without_depending_on_fonts() {
        for tex in [
            r"E = mc^2",
            r"\frac{-b \pm \sqrt{b^2-4ac}}{2a}",
            r"\sum_{i=1}^{n} i = \frac{n(n+1)}{2}",
            r"\int_0^\infty e^{-x^2}\,dx",
            r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}",
            r"\begin{aligned} a &= b \\ c &= d \end{aligned}",
            r"f(x) = \begin{cases} 1 & x > 0 \\ 0 & \text{otherwise} \end{cases}",
            r"\mathbb{R} \mathcal{L} \boldsymbol{\alpha}",
        ] {
            let math = typeset(tex, MathMode::Display).expect(tex);
            let svg = math.svg([200, 100, 50, 255]);
            assert!(!svg.contains("<text"), "{tex}");
            assert!(svg.contains("rgb(200,100,50)"), "{tex}");
            assert!(!svg.contains(TEXT_COLOR_SVG), "{tex}");
            parse_svg(&svg);
        }
    }

    #[test]
    fn measurements_match_the_svg() {
        let math = typeset(r"\frac{a}{b} + y", MathMode::Display).expect("valid formula");
        let size = parse_svg(&math.svg([0, 0, 0, 255])).size();
        let padding = 2. * math.padding();
        let expected_aspect_ratio =
            (math.width() + padding) / (math.ascent() + math.descent() + padding);
        assert!((size.width() / size.height() - expected_aspect_ratio).abs() < 0.001);
        assert!(math.ascent() > 0. && math.descent() > 0.);
    }

    #[test]
    fn display_math_is_larger_than_inline_math() {
        let inline = typeset(r"\sum_{i=1}^{n} i", MathMode::Inline).expect("valid formula");
        let display = typeset(r"\sum_{i=1}^{n} i", MathMode::Display).expect("valid formula");
        assert!(display.ascent() + display.descent() > inline.ascent() + inline.descent());
    }

    #[test]
    fn keeps_explicit_colors_and_applies_transparency() {
        let math = typeset(r"\textcolor{red}{x} + y", MathMode::Inline).expect("valid formula");
        let svg = math.svg([10, 20, 30, 128]);
        assert!(svg.contains("rgb(255,0,0)"));
        assert!(svg.contains("rgb(10,20,30)"));
        assert!(svg.contains(r#"<g opacity="0.5019608">"#));
        parse_svg(&svg);
    }

    #[test]
    fn rejects_invalid_and_oversized_formulas() {
        for tex in [
            "",
            "   ",
            r"\frac{1}{",
            r"\notacommand",
            "x^",
            r"\def\a{\a\a}\a",
            r"\rule{100000em}{100000em}",
        ] {
            assert!(typeset(tex, MathMode::Display).is_err(), "{tex:?}");
        }
        assert!(typeset(&"x".repeat(MAX_TEX_BYTES + 1), MathMode::Display).is_err());
    }

    #[test]
    fn deep_nesting_does_not_overflow_the_callers_stack() {
        // As small as the stacks of the background threads on macOS.
        let caller = std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(|| {
                for (opening, closing) in [
                    ("{", "}"),
                    (r"\frac{", "}{1}"),
                    (r"\sqrt{", "}"),
                    (r"\left(", r"\right)"),
                    (r"\begin{matrix}", r"\end{matrix}"),
                    (r"\hat", ""),
                ] {
                    for depth in [10, 20, 30, 200] {
                        let tex = format!("{}x{}", opening.repeat(depth), closing.repeat(depth));
                        // Whether the formula is accepted doesn't matter, only that
                        // typesetting returns.
                        drop(typeset(&tex, MathMode::Display));
                    }
                }
            })
            .expect("thread should start");
        assert!(caller.join().is_ok());
    }
}
