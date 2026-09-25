use aspectwrite::{
    parser,
    render::{self, Handwriting},
};

#[test]
fn render_small_stroke_fixture_and_report_missing_glyph() {
    let path = std::env::temp_dir().join(format!("aspectwrite-test-{}.json", std::process::id()));
    let fixture = serde_json::json!({
        "schema": "aspectwrite.handwriting", "version": 1,
        "glyphs": [
            {"key":"x","status":"complete","bbox":{"minX":0,"minY":-10,"maxX":50,"maxY":90},"strokes":[[{"x":0,"y":90},{"x":50,"y":-10}]]},
            {"key":"2","status":"complete","bbox":{"minX":0,"minY":0,"maxX":40,"maxY":80},"strokes":[[{"x":0,"y":80},{"x":40,"y":0}]]}
        ]
    });
    std::fs::write(&path, fixture.to_string()).unwrap();
    let hand = Handwriting::load(&path).unwrap();
    let png = render::png(&parser::parse(r"\frac{x^2}{x}").unwrap(), &hand).unwrap();
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    let image = tiny_skia::Pixmap::decode_png(&png).unwrap();
    assert!(image.width() > 60 && image.height() > 80);
    assert!(image.pixels().iter().any(|px| px.red() < 100));
    let large =
        render::png_with_seed_scaled(&parser::parse(r"\frac{x^2}{x}").unwrap(), &hand, 0, 3)
            .unwrap();
    let large_image = tiny_skia::Pixmap::decode_png(&large).unwrap();
    assert!((large_image.width() as i32 - 3 * image.width() as i32).abs() <= 2);
    assert!((large_image.height() as i32 - 3 * image.height() as i32).abs() <= 2);
    assert!(large_image.pixels().iter().any(|px| px.red() < 100));
    assert!(render::png_with_seed_scaled(&parser::parse("x").unwrap(), &hand, 0, 0).is_err());
    let x = parser::parse("x").unwrap();
    assert!(
        tiny_skia::Pixmap::decode_png(&render::png_with_seed_scaled(&x, &hand, 0, 16).unwrap())
            .unwrap()
            .width()
            > 900
    );
    assert!(render::png_with_seed_scaled(&x, &hand, 0, 17).is_err());
    let error = render::png(&parser::parse(r"x\mathbb{R}").unwrap(), &hand).unwrap_err();
    assert!(error.to_string().contains(r"\mathbb{R}"));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn variants_rotate_and_seed_is_reproducible() {
    let path =
        std::env::temp_dir().join(format!("aspectwrite-variants-{}.json", std::process::id()));
    let sample = |x| serde_json::json!({"baseline":0,"bbox":{"minX":0,"minY":0,"maxX":x,"maxY":90},"strokes":[[{"x":0,"y":90},{"x":x,"y":0}]]});
    let fixture = serde_json::json!({"schema":"aspectwrite.handwriting","version":2,"glyphs":[
        {"key":"a","status":"complete","variants":[sample(20),sample(40)]},
        {"key":"2","status":"complete","variants":[sample(20),sample(40)]}
    ]});
    std::fs::write(&path, fixture.to_string()).unwrap();
    let hand = Handwriting::load(&path).unwrap();
    for key in ["a", "2"] {
        let once = aspectwrite::render_png_with_seed(&format!("{key}{key}"), &hand, 0).unwrap();
        assert_eq!(
            once,
            aspectwrite::render_png_with_seed(&format!("{key}{key}"), &hand, 0).unwrap()
        );
        let swapped = aspectwrite::render_png_with_seed(&format!("{key}{key}"), &hand, 1).unwrap();
        assert_ne!(once, swapped);
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn saved_examples_parse() {
    for latex in [
        include_str!("../examples/stoichiometry.tex"),
        include_str!("../examples/integration-by-parts.tex"),
        include_str!("../examples/chemical-yields.tex"),
    ] {
        parser::parse(latex).unwrap();
    }
}

#[test]
fn full_user_file_if_available() {
    // Local integration test: set ASPECTWRITE_STROKES to an export path.
    // This also supports an ignored local profile; never check it into Git.
    let Ok(path) = std::env::var("ASPECTWRITE_STROKES") else {
        return;
    };
    let hand = Handwriting::load(std::path::Path::new(&path)).unwrap();
    for latex in [
        r"\Delta S = \int_{T_1}^{T_2} \frac{C_p(T)}{T}\,\mathrm{d}T",
        r"2\mathrm{H_2} + \mathrm{O_2} \rightarrow 2\mathrm{H_2O}",
        r"\mathrm{CaCO_3(s)} \xrightarrow{\Delta} \mathrm{CaO(s)} + \mathrm{CO_2(g)}",
        r"\frac{\partial u}{\partial t} = \alpha \nabla^2 u",
        r"\text{The heat added is }Q = mc\Delta T\text{.}",
        r"\begin{aligned}\frac{\mathrm{d}x}{\mathrm{d}t} &= v \\ \frac{\mathrm{d}v}{\mathrm{d}t} &= -\omega^2 x\end{aligned}",
        include_str!("../examples/stoichiometry.tex"),
        include_str!("../examples/integration-by-parts.tex"),
        include_str!("../examples/chemical-yields.tex"),
    ] {
        let png = aspectwrite::render_png(latex, &hand).unwrap_or_else(|e| panic!("{latex}: {e}"));
        assert!(png.starts_with(b"\x89PNG"));
    }
    let file: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let has_membership = [r"\in", r"\mathbb{R}"].iter().all(|key| {
        file["glyphs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|glyph| glyph["key"] == *key && glyph["status"] == "complete")
    });
    let result = aspectwrite::render_png(r"x\in\mathbb{R}", &hand);
    if has_membership {
        assert!(result.unwrap().starts_with(b"\x89PNG"));
    } else {
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("missing handwriting glyph")
        );
    }
}
