//! An agent harness over an ExcelGRAG corpus.
//!
//! `eg gui`'s chat is a fixed pipeline: condense, then `find → expand →
//! render`, then phrase. The model never chooses a tool. This crate inverts
//! that: the model drives the same thirteen tools `eg-mcp` serves —
//! `search`, `context`, `read_cells`, `precedents`, `recompute`, `what_if`…
//! — one call at a time, and the host sees every step land before the next
//! one is taken.
//!
//! It is built on Rig's [`AgentRun`](rig_agent::agent::AgentRun), a sans-IO state
//! machine: the harness asks it what to do next, makes the model call or the
//! tool calls itself, and feeds the result back. Nothing in the loop is
//! hidden inside a framework, which is what lets the host set the policy —
//! which tools may run, how many times, and at what cost — and lets a run be
//! serialised between steps and resumed later, in another process if need be.
//!
//! The tools are not redeclared here. [`tools`] iterates
//! `eg_mcp::tools::TOOLS` and dispatches through `eg_mcp::tools::call`,
//! exactly as `eg-gui`'s MCP bridge does, so there is one tool list and two
//! readers of it. Cell values reach the model on the terms
//! [`eg_mcp::State`] was opened on: `redact_values` at open time makes every
//! value its kind, and no tool here can talk its way past that.

pub mod blind_scan;
pub mod elide;
pub mod harness;
pub mod invalid_ref;
pub mod model;
pub mod policy;
pub mod preamble;
pub mod progress;
pub mod tools;

pub use harness::{CallRecord, Event, Harness, Outcome};
pub use model::{openai_compatible, OpenAiCompatible};
pub use policy::Policy;
