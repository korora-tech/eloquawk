//! Shared branding constants. Embedded icons arrive with the desktop integration (M14).

/// The application id used for the Wayland `app_id`, the desktop file, the icon name, the
/// notification icon and the keyring service.
pub const APP_ID: &str = "korora.tech.Eloquawk";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_is_reverse_dns() {
        assert_eq!(APP_ID, "korora.tech.Eloquawk");
        assert!(!APP_ID.contains('/'));
    }
}
