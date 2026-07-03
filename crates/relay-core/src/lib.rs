pub mod event;
pub mod ids;
pub mod model;
pub mod state;

pub use event::{fingerprint, EventKind, EventSource, RelayEvent};
pub use ids::{AgentKind, MachineId, SessionId, WindowId};
pub use model::{workspace_alias, ClaudeAgent, CodexAgent, MachineSnapshot, WindowEntry};
pub use state::{
    reduce_claude, reduce_codex, AskUserQuestion, ClaudeReduction, ClaudeState, CodexReduction,
    CodexState, QOption, Question,
};
