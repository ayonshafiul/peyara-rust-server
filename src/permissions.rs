//! OS permission handling for system input.
//!
//! Controlling the mouse/keyboard needs an explicit user grant on some
//! platforms. Today that is macOS: input injection requires the app to be
//! listed under System Settings → Privacy & Security → Accessibility.
//! Other platforms report [`PermissionState::Granted`] (Linux Wayland goes
//! through its own Remote Desktop portal prompt, handled in [`crate::input`]).

/// Accessibility/input permission status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionState {
    /// Input events will be delivered.
    Granted,
    /// The user must grant access (button opens the flow).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Missing,
}

/// Check the current input permission state. Never prompts by itself.
pub fn input_permission_state() -> PermissionState {
    #[cfg(target_os = "macos")]
    {
        macos::state()
    }
    #[cfg(not(target_os = "macos"))]
    {
        PermissionState::Granted
    }
}

/// Ask the OS to prompt for input permission. Returns the state observed
/// right after the prompt was shown. Only meaningful on macOS; elsewhere it
/// is a no-op returning [`PermissionState::Granted`].
///
/// Call this from an explicit button press, never from a poll loop: on macOS
/// it pops the system dialog every time access is missing.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn request_input_permission() -> PermissionState {
    #[cfg(target_os = "macos")]
    {
        macos::request()
    }
    #[cfg(not(target_os = "macos"))]
    {
        PermissionState::Granted
    }
}

/// Open the OS screen where the user grants input permission.
/// macOS: deep-links to the Accessibility pane. Elsewhere: no-op.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn open_accessibility_settings() {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .spawn();
    }
}
#[cfg(target_os = "macos")]
mod macos {
    use super::PermissionState;

    pub fn state() -> PermissionState {
        if macos_accessibility_client::accessibility::application_is_trusted() {
            PermissionState::Granted
        } else {
            PermissionState::Missing
        }
    }

    pub fn request() -> PermissionState {
        // Shows macOS's own "…would like to control this computer" dialog
        // with an "Open System Settings" button when access is missing.
        if macos_accessibility_client::accessibility::application_is_trusted_with_prompt() {
            PermissionState::Granted
        } else {
            // Still missing (user dismissed or hasn't toggled yet).
            state()
        }
    }
}
