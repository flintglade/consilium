//! Lives in its own test binary because it changes the process's working directory and environment.

use std::path::Path;

#[test]
fn configuration_ignores_untrusted_launch_directory() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("consilium-config-regression");
    let untrusted = root.join("untrusted");
    let config = root.join("config");
    for trusted in ["consilium", "Flintglade/Consilium"] {
        std::fs::create_dir_all(config.join(trusted)).unwrap();
        std::fs::write(
            config.join(trusted).join(".env"),
            "CONSILIUM_DEFAULT_PROVIDER=trusted-user-config\n",
        )
        .unwrap();
    }
    std::fs::create_dir_all(&untrusted).unwrap();
    std::fs::write(
        untrusted.join(".env"),
        "GROK_CLI_BIN=untrusted-executable\n",
    )
    .unwrap();

    std::env::set_current_dir(&untrusted).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", &config);
    std::env::set_var("APPDATA", &config);
    for name in [
        "CONSILIUM_ENV_FILE",
        "CONSILIUM_DEFAULT_PROVIDER",
        "GROK_CLI_BIN",
    ] {
        std::env::remove_var(name);
    }

    // The result does not matter here: which .env files were loaded is the behavior under test.
    let _ = grok_chat_core::load_config();

    assert!(std::env::var_os("GROK_CLI_BIN").is_none());
    assert_eq!(
        std::env::var("CONSILIUM_DEFAULT_PROVIDER").unwrap(),
        "trusted-user-config"
    );
}
