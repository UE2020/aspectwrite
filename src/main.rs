use std::{env, error::Error, path::Path};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = env::args().collect();
    if args.get(1).is_some_and(|arg| arg == "mcp") && args.len() <= 3 {
        use rmcp::ServiceExt;
        // Explicit argument wins, then environment override, then local default.
        // The default is relative to the server's working directory.
        let path = args
            .get(2)
            .cloned()
            .or_else(|| env::var("ASPECTWRITE_STROKES").ok())
            .unwrap_or_else(|| ".local/aspectwrite-strokes.json".into());
        let server = aspectwrite::server::AspectServer::new(Path::new(&path))?;
        server
            .serve(rmcp::transport::stdio())
            .await?
            .waiting()
            .await?;
        return Ok(());
    }
    if args.len() < 5 || args[1] != "render" {
        eprintln!(
            "Usage: aspectwrite render <strokes.json> <output.png> '<LaTeX>' [--seed <u64>] [--scale <1-16>]\n       aspectwrite mcp [strokes.json]  (defaults to .local/aspectwrite-strokes.json)"
        );
        std::process::exit(2);
    }
    let mut seed = 0;
    let mut scale = 1;
    let mut options = args[5..].chunks_exact(2);
    for option in &mut options {
        match option[0].as_str() {
            "--seed" => seed = option[1].parse::<u64>()?,
            "--scale" => scale = option[1].parse::<u32>()?,
            _ => return Err(format!("unknown render option {}", option[0]).into()),
        }
    }
    if !options.remainder().is_empty() {
        return Err("render option is missing its value".into());
    }
    let handwriting = aspectwrite::render::Handwriting::load(Path::new(&args[2]))?;
    let png = aspectwrite::render::png_with_seed_scaled(
        &aspectwrite::parser::parse(&args[4])?,
        &handwriting,
        seed,
        scale,
    )?;
    std::fs::write(&args[3], png)?;
    eprintln!("Wrote {}", args[3]);
    Ok(())
}
