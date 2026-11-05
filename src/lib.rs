//! Directional PostgreSQL to ClickHouse schema comparison and preflighted ALTERs.
pub mod app;
pub mod catalog;
pub mod compatibility;
pub mod config;
pub mod ddl;
pub mod diff;
pub mod discovery;
pub mod model;
pub mod report;
