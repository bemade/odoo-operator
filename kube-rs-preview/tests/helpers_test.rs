//! Unit tests for helper functions.

use odoo_operator::helpers::*;

#[test]
fn test_sanitise_uid_replaces_non_alphanumeric() {
    assert_eq!(sanitise_uid("abc-123-DEF"), "abc_123____");
    assert_eq!(sanitise_uid("abcdef0123456789"), "abcdef0123456789");
    assert_eq!(sanitise_uid(""), "");
}

#[test]
fn test_odoo_username_format() {
    assert_eq!(
        odoo_username("production", "my-odoo"),
        "odoo.production.my-odoo"
    );
    assert_eq!(odoo_username("default", "test"), "odoo.default.test");
}

#[test]
fn test_generate_password_length_and_hex() {
    let pw = generate_password();
    assert_eq!(pw.len(), 48); // 24 bytes → 48 hex chars
    assert!(pw.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn test_generate_password_uniqueness() {
    let pw1 = generate_password();
    let pw2 = generate_password();
    assert_ne!(pw1, pw2);
}

#[test]
fn test_sha256_hex_deterministic() {
    let hash1 = sha256_hex("hello world");
    let hash2 = sha256_hex("hello world");
    assert_eq!(hash1, hash2);
    assert_eq!(hash1.len(), 64); // SHA-256 → 32 bytes → 64 hex chars
}

#[test]
fn test_sha256_hex_different_inputs() {
    assert_ne!(sha256_hex("a"), sha256_hex("b"));
}

#[test]
fn test_build_odoo_conf_contains_required_keys() {
    let conf = build_odoo_conf(
        "odoo.ns.inst",
        "secret123",
        "admin_pw",
        "pg-host",
        5432,
        "odoo_db",
        &None,
    );

    assert!(conf.starts_with("[options]\n"));
    assert!(conf.contains("db_host = pg-host\n"));
    assert!(conf.contains("db_port = 5432\n"));
    assert!(conf.contains("db_name = odoo_db\n"));
    assert!(conf.contains("db_user = odoo.ns.inst\n"));
    assert!(conf.contains("db_password = secret123\n"));
    assert!(conf.contains("admin_passwd = admin_pw\n"));
    assert!(conf.contains("proxy_mode = True\n"));
    assert!(conf.contains("list_db = False\n"));
    assert!(conf.contains("http_port = 8069\n"));
}

#[test]
fn test_build_odoo_conf_with_extra_options() {
    let extra = Some(std::collections::BTreeMap::from([
        ("workers".to_string(), "4".to_string()),
        ("max_cron_threads".to_string(), "1".to_string()),
    ]));

    let conf = build_odoo_conf("u", "p", "a", "h", 5432, "d", &extra);

    assert!(conf.contains("workers = 4\n"));
    assert!(conf.contains("max_cron_threads = 1\n"));
}

#[test]
fn test_build_odoo_conf_prepends_standard_addons_path() {
    let conf = build_odoo_conf("u", "p", "a", "h", 5432, "d", &None);
    // The standard addons paths should be prepended.
    assert!(
        conf.contains("addons_path = /opt/odoo/addons,/opt/odoo/odoo/addons,/mnt/extra-addons\n")
    );
}

#[test]
fn test_build_odoo_conf_empty_admin_password_omitted() {
    let conf = build_odoo_conf("u", "p", "", "h", 5432, "d", &None);
    assert!(!conf.contains("admin_passwd"));
}
