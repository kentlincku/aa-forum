//! 手動煙霧測試：對真實 ACP agent 送一輪提示。
//!   cargo run -p aaf-runtime --example acp_smoke -- <cwd> "<prompt>" -- <acp command...>

use aaf_runtime::acp::AcpDriver;
use aaf_runtime::{Driver, DriverEvent};
use std::time::Instant;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sep = args.iter().position(|a| a == "--").ok_or("missing --")?;
    let (cwd, prompt, cmd) = (args[0].clone(), args[1].clone(), args[sep + 1..].to_vec());
    let d = AcpDriver::new(cmd, cwd.into(), vec![]);
    let t0 = Instant::now();
    d.start().await?;
    println!("started in {:?}", t0.elapsed());
    let mut rx = d.events();
    tokio::spawn(async move {
        while let Ok(e) = rx.recv().await {
            match e {
                DriverEvent::MessageChunk { .. } => {}
                other => println!("event: {}", serde_json::to_string(&other).unwrap()),
            }
        }
    });
    let t1 = Instant::now();
    let r = d.prompt(&prompt).await?;
    println!(
        "turn in {:?}: stop={} text={:?}",
        t1.elapsed(),
        r.stop_reason,
        r.text
    );
    d.stop().await?;
    Ok(())
}
