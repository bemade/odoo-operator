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

/// Script body without comment lines: the header block names required env
/// vars, and a script that only documents them is not the same as one that
/// expands them.
fn code_lines(body: &str) -> String {
    body.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

fn handles_secret(body: &str) -> bool {
    let code = code_lines(body);
    SECRET_MARKERS.iter().any(|m| code.contains(m))
}

/// Case-insensitive variant for the testing/ scripts, which read credentials
/// out of Kubernetes objects (`.data.password`, `adminPassword:`) rather than
/// from upper-case env vars.
fn handles_secret_any_case(body: &str) -> bool {
    let code = code_lines(body).to_lowercase();
    SECRET_MARKERS
        .iter()
        .any(|m| code.contains(&m.to_lowercase()))
}

/// Collect the `.sh` files under `dir` (recursing into subdirectories when
/// `recursive`), as paths relative to the crate root.
fn shell_scripts(dir: &Path, recursive: bool, out: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            if recursive {
                shell_scripts(&path, recursive, out);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("sh") {
            out.push(path);
        }
    }
}

fn tracing_offenders(scripts: &[std::path::PathBuf], secret: fn(&str) -> bool) -> Vec<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    scripts
        .iter()
        .filter(|p| {
            let body = fs::read_to_string(p).expect("read script");
            secret(&body) && enables_tracing(&body)
        })
        .map(|p| p.strip_prefix(root).unwrap_or(p).display().to_string())
        .collect()
}

#[test]
fn secret_handling_scripts_do_not_enable_shell_tracing() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts");
    let mut scripts = Vec::new();
    shell_scripts(&dir, false, &mut scripts);
    assert!(!scripts.is_empty(), "no shell scripts found under scripts/");

    let offenders = tracing_offenders(&scripts, handles_secret);
    assert!(
        offenders.is_empty(),
        "these scripts handle credentials but enable shell tracing, which would \
         expand those credentials into the container log: {offenders:?}"
    );
}

/// The local test-cluster scripts under testing/ are not embedded in Jobs,
/// but they do read cluster credentials (e.g. the Postgres superuser
/// password), and `set -x` would echo those into the terminal or a CI log.
#[test]
fn testing_scripts_do_not_enable_shell_tracing_with_secrets() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("testing");
    let mut scripts = Vec::new();
    shell_scripts(&dir, true, &mut scripts);
    assert!(!scripts.is_empty(), "no shell scripts found under testing/");

    let offenders = tracing_offenders(&scripts, handles_secret_any_case);
    assert!(
        offenders.is_empty(),
        "these testing scripts handle credentials but enable shell tracing, \
         which would expand those credentials into the log: {offenders:?}"
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

    assert!(handles_secret_any_case(
        "pw=\"$(kubectl get secret s -o jsonpath='{.data.password}')\""
    ));
    assert!(!handles_secret(
        "pw=\"$(kubectl get secret s -o jsonpath='{.data.password}')\""
    ));
    assert!(!handles_secret_any_case(
        "# reads the password\nkubectl get pods"
    ));
}
