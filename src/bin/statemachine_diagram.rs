//! Generate a Mermaid state diagram from the TRANSITIONS table.
//!
//! Usage:
//!   cargo run --bin statemachine_diagram                        # stdout
//!   cargo run --bin statemachine_diagram -- --out STATE_MACHINE.md

use odoo_operator::controller::state_machine::{TransitionAction, TRANSITIONS};
use std::path::PathBuf;

fn main() {
    let out: Option<PathBuf> = std::env::args()
        .skip_while(|a| a != "--out")
        .nth(1)
        .map(PathBuf::from);

    let md = generate();

    match out {
        Some(path) => {
            std::fs::write(&path, &md)
                .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
            eprintln!("wrote {}", path.display());
        }
        None => print!("{md}"),
    }
}

fn action_name(a: &TransitionAction) -> &'static str {
    match a {
        TransitionAction::MarkDbInitialized => "MarkDbInitialized",
        TransitionAction::CompleteInitJob => "CompleteInitJob",
        TransitionAction::FailInitJob => "FailInitJob",
        TransitionAction::CompleteRestoreJob => "CompleteRestoreJob",
        TransitionAction::FailRestoreJob => "FailRestoreJob",
        TransitionAction::CompleteUpgradeJob => "CompleteUpgradeJob",
        TransitionAction::FailUpgradeJob => "FailUpgradeJob",
        TransitionAction::CompleteBackupJob => "CompleteBackupJob",
        TransitionAction::FailBackupJob => "FailBackupJob",
    }
}

fn generate() -> String {
    let mut out = String::new();
    out.push_str("# OdooInstance State Machine\n\n");
    out.push_str(
        "Auto-generated from the `TRANSITIONS` table in `controller/state_machine.rs`.\n\n",
    );
    out.push_str("```mermaid\nstateDiagram-v2\n");
    out.push_str("    [*] --> Provisioning\n\n");

    // One edge per transition — guards make each edge distinct.
    let mut current_from = String::new();
    for t in TRANSITIONS.iter() {
        let from = format!("{}", t.from);
        let to = format!("{}", t.to);

        if from != current_from {
            if !current_from.is_empty() {
                out.push('\n');
            }
            current_from = from.clone();
        }

        let guard_part = if t.guard_name.is_empty() {
            String::new()
        } else {
            format!("[{}]", t.guard_name)
        };

        let action_part = if t.actions.is_empty() {
            String::new()
        } else {
            let names: Vec<&str> = t.actions.iter().map(action_name).collect();
            format!("/ {}", names.join(", "))
        };

        let label = match (guard_part.is_empty(), action_part.is_empty()) {
            (true, true) => String::new(),
            (false, true) => format!(" : {guard_part}"),
            (true, false) => format!(" : {action_part}"),
            (false, false) => format!(" : {guard_part} {action_part}"),
        };

        out.push_str(&format!("    {from} --> {to}{label}\n"));
    }

    out.push_str("```\n");
    out
}
