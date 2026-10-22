//! Windows Hello check before the keystore key is read. Touch ID is not wired up yet, so other
//! platforms report [`Presence::Unavailable`]. Both calls block: run them off the UI thread.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Verified,
    /// The user cancelled or failed the check.
    Declined,
    /// No Windows Hello on this machine, or it is not set up for this user.
    Unavailable,
}

/// Whether [`verify_presence`] can ask on this machine.
pub fn presence_available() -> bool {
    imp::available()
}

/// Asks the user to confirm with biometrics or the device PIN; `message` is shown in the prompt.
pub fn verify_presence(message: &str) -> Presence {
    imp::verify(message)
}

#[cfg(windows)]
mod imp {
    use windows::Security::Credentials::UI::{
        UserConsentVerificationResult as Result, UserConsentVerifier,
        UserConsentVerifierAvailability,
    };
    use windows::core::HSTRING;

    use super::Presence;

    pub fn available() -> bool {
        UserConsentVerifier::CheckAvailabilityAsync()
            .and_then(|op| op.join())
            .is_ok_and(|a| a == UserConsentVerifierAvailability::Available)
    }

    pub fn verify(message: &str) -> Presence {
        match UserConsentVerifier::RequestVerificationAsync(&HSTRING::from(message))
            .and_then(|op| op.join())
        {
            Ok(Result::Verified) => Presence::Verified,
            Ok(Result::Canceled | Result::RetriesExhausted) => Presence::Declined,
            Ok(_) | Err(_) => Presence::Unavailable,
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Presence;

    pub fn available() -> bool {
        false
    }

    pub fn verify(_message: &str) -> Presence {
        Presence::Unavailable
    }
}
