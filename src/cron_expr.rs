//! Cron expression normalization, parsing, display, and next-run calculation.
//!
//! TinyButler uses `croner` as the single source of truth so scheduling and
//! human-readable descriptions cannot disagree.

use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use croner::Cron;

/// Normalize five-field cron expressions by adding a leading seconds field.
pub fn normalize_cron(expr: &str) -> String {
    let fields = expr.split_whitespace().count();
    if fields == 5 {
        format!("0 {expr}")
    } else {
        expr.to_string()
    }
}

/// Parse a cron expression after applying TinyButler's normalization rule.
pub fn parse_cron(expr: &str) -> Result<Cron> {
    let normalized = normalize_cron(expr);
    Cron::from_str(&normalized).with_context(|| format!("invalid cron expression: {expr}"))
}

/// Return the next local-time occurrence after `after`.
pub fn next_run_after(expr: &str, after: DateTime<Local>) -> Result<DateTime<Local>> {
    parse_cron(expr)?
        .find_next_occurrence(&after, false)
        .with_context(|| format!("failed to find next occurrence for cron expression: {expr}"))
}

/// Return a human-readable English description for a cron expression.
pub fn describe_schedule(expr: &str) -> String {
    match parse_cron(expr) {
        Ok(cron) => cron.describe(),
        Err(_) => "unrecognized schedule".to_string(),
    }
}

/// Format the schedule line shown in CLI and Telegram outputs.
pub fn format_schedule_for_display(expr: &str) -> String {
    format!("{} ({})", describe_schedule(expr), expr)
}
