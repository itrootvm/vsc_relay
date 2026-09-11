use crate::config::SemanticConfig;
use anyhow::{bail, Result};
use relay_compass::{SemanticFacts, SemanticInputFrame};

const UNAVAILABLE: &str = "the local semantic model is not part of the static Linux build; use the Ollama, OpenAI-compatible or agent CLI backend";

pub fn classify(
    _config: &SemanticConfig,
    _frames: &[SemanticInputFrame],
) -> Result<Vec<SemanticFacts>> {
    bail!(UNAVAILABLE)
}

pub fn status(_config: &SemanticConfig) -> Result<String> {
    bail!(UNAVAILABLE)
}
