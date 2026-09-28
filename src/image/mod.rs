//! Image generation: the one thing an Implementer may use that is not its
//! own coding runtime.
//!
//! This module is deliberately not a provider. A provider is a native
//! coding-agent CLI that performs a role; image generation is a tool that
//! role may call. The coding runtime asks for an image through a run-scoped
//! MCP shim; the Senate process owns authorization, the credential, the
//! per-run bound, filesystem placement inside the managed worktree, and the
//! evidence row. The generation itself sits behind [`ImageGenerator`]; the
//! production backend is the user's own Codex CLI and its built-in
//! `image_gen` tool, so no vendor API is called and no API key exists.
//!
//! Boundary (one arrow per process):
//!
//! ```text
//! native CLI --stdio MCP--> `senate __image-tool` --unix socket--> ImageToolHost
//!                                                                     |
//!                                                            ImageToolService
//!                                                       authorize · bound · path · evidence
//!                                                                     |
//!                                                              ImageGenerator
//!                                              (Codex CLI built-in image_gen | Fake)
//! ```

mod codex;
mod fake;
mod host;
pub(crate) mod mcp;
mod path;
mod png;
mod service;

use std::fmt;

pub use codex::{CODEX_IMAGE_MODEL, CodexImageGenerator, backend_available};
pub use fake::FakeImageGenerator;
pub use host::{ImageToolHost, ImageToolServerCommand, MCP_SERVER_NAME, SHIM_SUBCOMMAND};
pub use mcp::{TOOL_NAME, run_stdio_server};
pub use path::{OutputPathError, validate_output_path};
pub use service::{
    ImageToolCall, ImageToolError, ImageToolErrorCode, ImageToolScope, ImageToolService,
    ImageToolSuccess, MAX_PROMPT_BYTES, evidence_directory,
};

/// Output size the agent may request. Kept to three aspect presets plus the
/// backend's own default so a run never asks for an arbitrary resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ImageSize {
    #[default]
    Auto,
    Square1024,
    Landscape1536x1024,
    Portrait1024x1536,
}

impl ImageSize {
    /// The value the agent writes and the backend sends.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Square1024 => "1024x1024",
            Self::Landscape1536x1024 => "1536x1024",
            Self::Portrait1024x1536 => "1024x1536",
        }
    }

    /// Parses the agent's value; anything else is a typed tool error.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "1024x1024" => Some(Self::Square1024),
            "1536x1024" => Some(Self::Landscape1536x1024),
            "1024x1536" => Some(Self::Portrait1024x1536),
            _ => None,
        }
    }
}

/// Rendering quality the agent may request. `Medium` is the default so an
/// agent that does not care does not buy the most expensive tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ImageQuality {
    Low,
    #[default]
    Medium,
    High,
}

impl ImageQuality {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

/// Backend-neutral generation request. The output is always PNG in v1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRequest {
    pub prompt: String,
    pub size: ImageSize,
    pub quality: ImageQuality,
    pub transparent_background: bool,
}

/// One generated PNG plus the backend's own account of it.
#[derive(Clone, PartialEq, Eq)]
pub struct GeneratedImage {
    /// Raw PNG bytes exactly as the backend returned them.
    pub png: Vec<u8>,
    /// Backend identity (`openai`, `fake`).
    pub backend: &'static str,
    /// Concrete model the backend used. Owned here, never by the domain.
    pub model: String,
    /// Backend request/response identifier when one was exposed.
    pub response_id: Option<String>,
}

impl fmt::Debug for GeneratedImage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GeneratedImage")
            .field("png_bytes", &self.png.len())
            .field("backend", &self.backend)
            .field("model", &self.model)
            .field("response_id", &self.response_id)
            .finish()
    }
}

/// Why a backend could not produce an image. Every variant is safe to show
/// the coding agent; none carries a credential.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ImageBackendError {
    #[error("image backend is not configured: {0}")]
    NotConfigured(String),
    #[error("image backend rejected the request: {0}")]
    Rejected(String),
    #[error("image backend is unreachable: {0}")]
    Network(String),
    #[error("image backend returned an invalid response: {0}")]
    InvalidResponse(String),
}

/// The narrow vendor boundary. One request, one PNG, or a typed failure.
pub trait ImageGenerator: Send + Sync {
    /// Human-readable backend identity for doctor/evidence.
    fn backend(&self) -> &'static str;

    /// The concrete model this backend will send, for doctor output and
    /// evidence. Not a domain concept.
    fn model(&self) -> &str;

    /// Generates exactly one image.
    ///
    /// # Errors
    /// Returns a typed backend failure; never panics on vendor output.
    fn generate(&self, request: &ImageRequest) -> Result<GeneratedImage, ImageBackendError>;
}
