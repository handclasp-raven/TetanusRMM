//! Validates the update signing key baked in via `RMM_UPDATE_PUBKEY`, so a
//! typo fails the build instead of producing an agent that cannot update.

fn main() {
    println!("cargo:rerun-if-env-changed=RMM_UPDATE_PUBKEY");
    if let Ok(key) = std::env::var("RMM_UPDATE_PUBKEY") {
        let valid = key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit());
        assert!(
            valid,
            "RMM_UPDATE_PUBKEY must be 64 hex characters (the contents of update-keys/update.pub)"
        );
    }
}
