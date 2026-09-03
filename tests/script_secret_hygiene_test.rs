//! Guards the job scripts against re-introducing credential disclosure.
//!
//! The job scripts are embedded verbatim (`include_str!`) and run as the
//! container command, so their stdout/stderr becomes the pod log and is shipped
//! to the cluster log aggregator.  `set -x` makes the shell echo every command
//! *after* variable expansion, which writes any secret held in the environment
//! straight into that log — e.g. `+ PGPASSWORD=<value> pg_dump ...`.
//!
//! Any script that handles a credential must therefore not enable tracing.
//! Scripts that handle none may keep `set -x` for debuggability.

use std::fs;
use std::path::Path;

/// Env-var name fragments that indicate a script handles a credential.
const SECRET_MARKERS: &[&str] = &[
    "PASSWORD",
    "PASSWD",
    "SECRET_ACCESS_KEY",
    "ACCESS_KEY_ID",
    "master_pwd",
    "TOKEN",
];

/// True when the script enables shell tracing (`set -x`, `set -ex`, `set -eux`…).
fn enables_tracing(body: &str) -> bool {
    body.lines()
        .map(str::trim)
        .filter(|l| l.starts_with("set "))
        .any(|l| {
            l.split_whitespace()
                .skip(1)
                .filter(|tok| tok.starts_with('-') && !tok.starts_with("--"))
                .any(|tok| tok.contains('x'))
        })
}

fn handles_secret(body: &str) -> bool {
    // Ignore comment lines: the header block names required env vars, and a
    // script that only documents them is not the same as one that expands them.
    let code: String = body
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    SECRET_MARKERS.iter().any(|m| code.contains(m))
}

#[test]
fn secret_handling_scripts_do_not_enable_shell_tracing() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts");
    let mut offenders = Vec::new();
    let mut checked = 0;

    for entry in fs::read_dir(&dir).expect("scripts/ directory") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("sh") {
            continue;
        }
        let body = fs::read_to_string(&path).expect("read script");
        checked += 1;
        if handles_secret(&body) && enables_tracing(&body) {
            offenders.push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }

    assert!(checked > 0, "no shell scripts found under scripts/");
    assert!(
        offenders.is_empty(),
        "these scripts handle credentials but enable shell tracing, which would \
         expand those credentials into the container log: {offenders:?}"
    );
}

#[test]
fn tracing_and_secret_detection_recognise_the_shapes_they_guard() {
    assert!(enables_tracing("set -ex"));
    assert!(enables_tracing("set -x"));
    assert!(enables_tracing("  set -eux -o pipefail"));
    assert!(!enables_tracing("set -e"));
    assert!(!enables_tracing("set -euo pipefail"));
    assert!(!enables_tracing("set -eu"));

    assert!(handles_secret("export PGPASSWORD=$PASSWORD"));
    assert!(handles_secret("mc alias set d \"$AWS_SECRET_ACCESS_KEY\""));
    assert!(!handles_secret(
        "#   HOST, PASSWORD — connection\nls -lh /workspace"
    ));
}
