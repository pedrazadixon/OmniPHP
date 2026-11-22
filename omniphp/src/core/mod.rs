//! Everything that is not UI: discovery, configuration, config-file
//! generation and service supervision. Nothing in here imports GPUI, so it can
//! be unit-tested and driven from the command line.

pub mod apache;
pub mod config;
pub mod downloads;
pub mod engine;
pub mod hosts;
pub mod mysql;
pub mod paths;
pub mod php;
pub mod proc;
pub mod quickapp;
pub mod runtimes;
pub mod services;
pub mod share;
pub mod sites;
pub mod ssl;
pub mod startup;
pub mod system;
pub mod tools;
