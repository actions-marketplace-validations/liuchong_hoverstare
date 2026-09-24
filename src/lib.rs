//! HoverStare — AI code review bot (specs/ is the single source of truth)

pub mod agent;
pub mod cli;
pub mod config;
pub mod devagent;
pub mod develop;
pub mod devqueue;
pub mod diff;
pub mod event;
pub mod findings;
pub mod git;
pub mod github;
pub mod i18n;
pub mod instructions;
pub mod mention;
pub mod orchestrator;
pub mod output;
pub mod pipeline;
pub mod prompt;
pub mod report;
pub mod rules;
pub mod sanitize;
pub mod serve;
pub mod state;
pub mod units;
