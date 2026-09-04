//! Briefing: a paced browser briefing surface for coding agents, exposed as a CLI,
//! an MCP server (stdio or streamable HTTP), and a shared hub.

pub mod assets;
pub mod backend;
pub mod browser;
pub mod content;
pub mod guidance;
pub mod http;
pub mod hub;
pub mod mcp;
pub mod response;
pub mod store;
pub mod tailscale;
pub mod tls;

pub use backend::{Backend, BindMode, Created, LocalBackend, RemoteBackend, Site, SiteOptions};
pub use content::Briefing;
pub use hub::{BriefingStatus, Hub, HubConfig};
pub use response::{BriefingOutcome, BriefingResponse, Outcome};
