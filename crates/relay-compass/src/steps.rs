use crate::health::{SOURCE_AGENT, SOURCE_DELEGATE, SOURCE_RUNTIME, SOURCE_TOOL, SOURCE_USER};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepRole {
    User,
    Assistant,
    ToolUse,
    ToolResult,

    DelegateResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolKind {
    Inspect,
    Search,
    Execute,
    Modify,

    Delegate,
    #[default]
    Other,
}

impl StepRole {
    pub fn source_class(self) -> u8 {
        match self {
            StepRole::User => SOURCE_USER,
            StepRole::Assistant => SOURCE_AGENT,
            StepRole::ToolUse => SOURCE_TOOL,
            StepRole::ToolResult => SOURCE_RUNTIME,
            StepRole::DelegateResult => SOURCE_DELEGATE,
        }
    }
}

pub type SourceTurnId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserOrigin {
    #[default]
    Human,
    Controller,
    ToolAnswer,
    DelegateNotification,
    SystemContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContextFlags(u16);

impl ContextFlags {
    pub const IDE_OPENED_FILE: Self = Self(1 << 0);
    pub const IDE_SELECTION: Self = Self(1 << 1);
    pub const IDE_DIAGNOSTICS: Self = Self(1 << 2);
    pub const LOCAL_COMMAND: Self = Self(1 << 3);
    pub const SYSTEM_REMINDER: Self = Self(1 << 4);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u16 {
        self.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn contains(self, other: Self) -> bool {
        other.0 != 0 && self.0 & other.0 == other.0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

impl std::ops::BitOr for ContextFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SemanticStep {
    pub index: u32,
    pub role: StepRole,
    pub text: String,
    pub source_class: u8,
    pub tool_name: Option<String>,

    pub correlation_id: Option<String>,
    pub tool_kind: ToolKind,
    pub tool_target: Option<String>,
    pub is_error: bool,

    pub source_turn_id: SourceTurnId,
    pub block_index: u16,
    pub user_origin: UserOrigin,
    pub context_flags: ContextFlags,
}

impl SemanticStep {
    pub fn new(index: u32, role: StepRole, text: String) -> Self {
        SemanticStep {
            index,
            role,
            source_class: role.source_class(),
            text,
            tool_name: None,
            correlation_id: None,
            tool_kind: ToolKind::Other,
            tool_target: None,
            is_error: false,
            source_turn_id: index,
            block_index: 0,
            user_origin: UserOrigin::Human,
            context_flags: ContextFlags::empty(),
        }
    }
}
