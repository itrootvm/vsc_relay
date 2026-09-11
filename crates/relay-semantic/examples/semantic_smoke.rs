use relay_semantic::check;
use relay_semantic::config::SemanticConfig;

#[tokio::main]
async fn main() {
    let cli = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "cursor".to_string());
    let model = std::env::args().nth(2);
    let mut config = SemanticConfig::default();
    config.apply_preset(&cli).expect("preset");
    config.cli_model = model.filter(|m| !m.is_empty());
    config.timeout_secs = 120;
    println!(
        "backend={:?} cli_provider={:?}",
        config.backend,
        config.cli_provider()
    );
    if let Some(disc) = config.off_machine_disclosure() {
        println!("disclosure: {disc}");
    }
    match check(&config, None).await {
        Ok(msg) => println!("OK: {msg}"),
        Err(e) => println!("ERR: {e:#}"),
    }
}
