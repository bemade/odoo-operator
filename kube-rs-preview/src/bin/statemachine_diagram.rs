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

fn generate() -> String {
    let mut out = String::new();
    out.push_str("# OdooInstance State Machine\n\n");
    out.push_str("Auto-generated from the `TRANSITIONS` table in `controller/state_machine.rs`.\n\n");
    out.push_str("```mermaid\nstateDiagram-v2\n");

    // Start arrow.
    out.push_str("    [*] --> Provisioning\n\n");

    // Group transitions by `from` for readability.
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

        // Build label from action (if any).
        let label = match t.action {
            Some(TransitionAction::MarkDbInitialized) => " : / MarkDbInitialized",
            None => "",
        };

        out.push_str(&format!("    {from} --> {to}{label}\n"));
    }

    out.push_str("```\n");
    out
}
