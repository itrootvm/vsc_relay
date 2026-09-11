use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const BUILTIN_MODEL_ID: &str = "minilm-multilingual-nli-fp16-v2";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SemanticBackend {
    Off,
    #[default]
    Local,
    Ollama,
    OpenAiCompatible,

    AgentCli,
}

impl SemanticBackend {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Local => "local",
            Self::Ollama => "ollama",
            Self::OpenAiCompatible => "openai_compatible",
            Self::AgentCli => "agent_cli",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "off" | "none" => Some(Self::Off),
            "local" | "builtin" => Some(Self::Local),
            "ollama" => Some(Self::Ollama),
            "openai" | "openai_compatible" | "compatible" | "custom" | "openrouter" | "nvidia"
            | "nim" => Some(Self::OpenAiCompatible),
            "cli" | "agent_cli" | "claude" | "codex" | "gemini" | "cursor" | "cursor_agent"
            | "antigravity" | "agy" => Some(Self::AgentCli),
            _ => None,
        }
    }
}

pub fn canonical_cli(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "gemini" => Some("gemini"),
        "cursor" | "cursor_agent" => Some("cursor"),
        "antigravity" | "agy" => Some("antigravity"),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticConfig {
    #[serde(default)]
    pub backend: SemanticBackend,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_dir: Option<PathBuf>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_model: Option<String>,
    #[serde(default = "default_confidence")]
    pub min_confidence: f32,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub allow_uncalibrated_steer: bool,
}

impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            backend: SemanticBackend::Local,
            model: default_model(),
            endpoint: None,
            local_dir: None,
            cli_model: None,
            min_confidence: default_confidence(),
            timeout_secs: default_timeout(),
            allow_uncalibrated_steer: false,
        }
    }
}

fn default_model() -> String {
    BUILTIN_MODEL_ID.to_string()
}

fn default_confidence() -> f32 {
    0.78
}

fn default_timeout() -> u64 {
    45
}

impl SemanticConfig {
    pub fn model_dir(&self) -> PathBuf {
        self.local_dir.clone().unwrap_or_else(builtin_model_dir)
    }

    pub fn normalized_endpoint(&self) -> Option<String> {
        let raw = self.endpoint.as_deref()?.trim().trim_end_matches('/');
        (!raw.is_empty()).then(|| raw.to_string())
    }

    pub fn cli_provider(&self) -> Option<&'static str> {
        (self.backend == SemanticBackend::AgentCli)
            .then(|| canonical_cli(&self.model))
            .flatten()
    }

    pub fn sends_off_machine(&self) -> bool {
        match self.backend {
            SemanticBackend::Off | SemanticBackend::Local => false,
            SemanticBackend::OpenAiCompatible | SemanticBackend::AgentCli => true,
            SemanticBackend::Ollama => {
                let endpoint = self.normalized_endpoint().unwrap_or_default();
                !(endpoint.is_empty()
                    || endpoint.contains("localhost")
                    || endpoint.contains("127.0.0.1")
                    || endpoint.contains("::1"))
            }
        }
    }

    pub fn off_machine_disclosure(&self) -> Option<String> {
        if let Some(cli) = self.cli_provider() {
            return Some(format!(
                "transcript text (goal, messages, tool output) will be sent to the '{cli}' CLI, which relays it to that provider's cloud service; use provider 'local' to keep everything on-device"
            ));
        }
        self.sends_off_machine().then(|| {
            let target = self
                .normalized_endpoint()
                .unwrap_or_else(|| "the configured endpoint".to_string());
            format!(
                "transcript text (goal, messages, tool output) will be sent off this machine to {target}; use provider 'local' to keep everything on-device"
            )
        })
    }

    pub fn apply_preset(&mut self, preset: &str) -> anyhow::Result<()> {
        match preset.trim().to_ascii_lowercase().as_str() {
            "off" => self.backend = SemanticBackend::Off,
            "local" | "builtin" => {
                self.backend = SemanticBackend::Local;
                self.model = BUILTIN_MODEL_ID.to_string();
                self.endpoint = None;
            }
            "ollama" => {
                self.backend = SemanticBackend::Ollama;
                self.endpoint = Some("http://localhost:11434".to_string());
                if self.model == BUILTIN_MODEL_ID {
                    self.model.clear();
                }
            }
            "openrouter" => {
                self.backend = SemanticBackend::OpenAiCompatible;
                self.endpoint = Some("https://openrouter.ai/api/v1".to_string());
                if self.model == BUILTIN_MODEL_ID {
                    self.model.clear();
                }
            }
            "nvidia" | "nim" => {
                self.backend = SemanticBackend::OpenAiCompatible;
                self.endpoint = Some("https://integrate.api.nvidia.com/v1".to_string());
                if self.model == BUILTIN_MODEL_ID {
                    self.model.clear();
                }
            }
            "openai" | "openai-compatible" | "openai_compatible" | "custom" => {
                self.backend = SemanticBackend::OpenAiCompatible;
                if self.model == BUILTIN_MODEL_ID {
                    self.model.clear();
                }
            }
            other if canonical_cli(other).is_some() => {
                self.backend = SemanticBackend::AgentCli;
                self.model = canonical_cli(other).unwrap().to_string();
                self.endpoint = None;
            }
            other => anyhow::bail!(
                "unknown semantic provider '{other}' (off|local|ollama|openrouter|nvidia|openai-compatible|claude|codex|gemini|cursor|antigravity)"
            ),
        }
        Ok(())
    }
}

pub fn builtin_model_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".vsc-relay")
        .join("models")
        .join("semantic")
        .join(BUILTIN_MODEL_ID)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_machine_disclosure_only_for_external_backends() {
        let mut config = SemanticConfig::default();
        assert_eq!(config.backend, SemanticBackend::Local);
        assert!(!config.sends_off_machine());
        assert!(config.off_machine_disclosure().is_none());

        config.apply_preset("ollama").unwrap();
        assert!(
            !config.sends_off_machine(),
            "localhost ollama stays on-machine"
        );
        assert!(config.off_machine_disclosure().is_none());

        config.apply_preset("openrouter").unwrap();
        assert!(config.sends_off_machine());
        let disclosure = config.off_machine_disclosure().unwrap();
        assert!(disclosure.contains("off this machine"));
        assert!(disclosure.contains("openrouter.ai"));

        config.apply_preset("claude").unwrap();
        assert_eq!(config.backend, SemanticBackend::AgentCli);
        assert!(config.sends_off_machine(), "cloud CLI leaves the machine");
        let disclosure = config.off_machine_disclosure().unwrap();
        assert!(disclosure.contains("'claude' CLI"));
        assert!(disclosure.contains("cloud service"));

        config.apply_preset("off").unwrap();
        assert!(!config.sends_off_machine());
    }

    #[test]
    fn cli_preset_aliases_resolve_to_canonical_id() {
        let mut config = SemanticConfig::default();
        config.apply_preset("cursor-agent").unwrap();
        assert_eq!(config.backend, SemanticBackend::AgentCli);
        assert_eq!(config.model, "cursor");
        assert_eq!(config.cli_provider(), Some("cursor"));

        config.apply_preset("agy").unwrap();
        assert_eq!(config.model, "antigravity");
        assert_eq!(config.cli_provider(), Some("antigravity"));
    }

    #[test]
    fn presets_share_generic_openai_transport() {
        let mut config = SemanticConfig::default();
        config.apply_preset("openrouter").unwrap();
        assert_eq!(config.backend, SemanticBackend::OpenAiCompatible);
        assert_eq!(
            config.endpoint.as_deref(),
            Some("https://openrouter.ai/api/v1")
        );
        config.apply_preset("nvidia").unwrap();
        assert_eq!(
            config.endpoint.as_deref(),
            Some("https://integrate.api.nvidia.com/v1")
        );
    }
}
