//! The synthetic lab slips without Python.
//!
//!     cargo run -p ownhealth-ingest-pdf --example labslips -- generate OUTDIR [--injection]
//!
//! writes OUTDIR/<slip>.pdf and OUTDIR/<slip>.truth (and <slip>.marker for injection fixtures).
#[path = "../tests/support/mod.rs"]
mod support;

use support::generate;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let inj = args.iter().any(|a| a == "--injection");
    let rest: Vec<&String> = args.iter().filter(|a| *a != "--injection").collect();
    match rest.first().map(|s| s.as_str()) {
        Some("generate") => {
            let out = std::path::PathBuf::from(rest.get(1).map_or("labslips-out", |s| s.as_str()));
            std::fs::create_dir_all(&out)?;
            let slips = if inj {
                generate::injection_fixtures()
            } else {
                let mut s = generate::all_slips();
                s.extend(generate::no_text_layer());
                s.extend(generate::injection_fixtures());
                s
            };
            for (name, pdf, truth) in &slips {
                std::fs::write(out.join(format!("{name}.pdf")), pdf)?;
                std::fs::write(
                    out.join(format!("{name}.truth")),
                    serde_json::to_string_pretty(truth)? + "\n",
                )?;
                if truth.get("expect").is_some() {
                    std::fs::write(out.join(format!("{name}.marker")), "CANARY\n")?;
                }
            }
            println!("wrote {} PDFs to {}", slips.len(), out.display());
            Ok(())
        }
        _ => anyhow::bail!("usage: labslips generate OUTDIR [--injection]"),
    }
}
