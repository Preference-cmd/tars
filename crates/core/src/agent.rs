//! Homogeneous per-agent execution unit domain types and runtime state.
//!
//! Every agent inside a multi-agent session (both the entry agent and any
//! spawned child agents) shares the same [`AgentDescriptor`], frozen
//! [`SessionDefinition`](crate::definition::SessionDefinition), and
//! `AgentUnit` execution model.

mod types;
mod unit;

pub use types::{
    AgentDescriptor, AgentMaterialSnapshot, AgentMaterialVersion, AgentRepairInspection,
    AgentRepairPlan, AgentSnapshot,
};
pub(crate) use unit::AgentUnit;
