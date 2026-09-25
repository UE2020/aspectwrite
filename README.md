# aspectwrite

Render a closed [LaTeX subset](docs/latex-subset.md) as handwritten PNGs. The Rust renderer supports multiple samples per letter and digit, deterministic variation, and an MCP server. An offline [handwriting collector](handwriting-collector.html) creates the stroke profiles used by the renderer.

## Demo

The animation renders the same chemical reaction with different seeds, showcasing the renderer's handwritten letter variations:

![Handwritten chemical equation rendered with varied letterforms](assets/chemical-equation.gif)

Reaction source: `\text{isoborneol }C_{10}H_{18}O\xrightarrow{H^{+},\,\Delta}\text{camphene }C_{10}H_{16}+H_2O`.

## Build and render

Install [Rust](https://www.rust-lang.org/tools/install), then:

```sh
cargo build --release
cargo run --release -- render path/to/strokes.json equation.png '\frac{\mathrm{d}y}{\mathrm{d}x}=ky' --seed 7
```

A stroke profile is not bundled; create or import one with `handwriting-collector.html` and export it as JSON. The renderer also accepts v1 profiles. Latin letters and digits support 1–3 samples; other glyphs need one. A fixed `--seed` produces byte-identical output: it selects the starting sample for repeated glyphs and seeds the small per-instance variation in size, slant, baseline, and stroke weight. See [the stroke-file format](docs/stroke-file.md) and [examples](examples/README.md).

For a higher-resolution PNG with the same layout, add `--scale 3` to the render command (supported values: 1–16). Large images are still subject to a pixel-count limit.

## MCP server

Build with `cargo build --release` and run `cargo run --release -- mcp path/to/strokes.json`. The server speaks stdio JSON-RPC and exposes `render_latex`, returning a PNG image. Alternatively set `ASPECTWRITE_STROKES` to a profile path; if neither is supplied, the server looks for `.local/aspectwrite-strokes.json` relative to its working directory. See [Pi integration](.pi/extensions/aspectwrite.ts) for the included Pi extension.

## Verify

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets
python tests/mcp_client.py --local
node --experimental-strip-types tests/pi-extension.mjs
node tests/collector-input.cjs
node tests/collector.cjs path/to/old/v1-export.json
```
