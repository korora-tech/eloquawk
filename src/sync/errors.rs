use super::*;

/// Why a Google call failed.
pub(super) enum CallError {
    /// The account has no usable token (it was just signed out, or the client is rejected).
    Auth,
    /// The account is no longer signed in at all; skip it quietly.
    Gone,
    /// The token endpoint could not be reached (or failed with a 5xx).
    Network,
    Api(ApiError),
}

impl CallError {
    /// The text a failed thread shows in the reader.
    pub(super) fn reader_message(&self) -> String {
        match self {
            CallError::Auth | CallError::Gone => SIGNED_OUT.into(),
            CallError::Network => NETWORK_PROBLEM.into(),
            CallError::Api(ApiError::TooLarge) => "Message too large to open".into(),
            CallError::Api(ApiError::NotFound) => "This conversation is no longer in Gmail.".into(),
            CallError::Api(e) => e.to_string(),
        }
    }
}

/// The text a failed change shows (the store rolls the change back with it).
pub(super) fn change_message(e: &CallError) -> String {
    match e {
        CallError::Auth | CallError::Gone => SIGNED_OUT.into(),
        CallError::Network | CallError::Api(ApiError::Network(_)) => NETWORK_PROBLEM.into(),
        CallError::Api(ApiError::NotFound) => "This conversation is no longer in Gmail.".into(),
        CallError::Api(e) => e.to_string(),
    }
}

/// How a failed send is reported (SPEC 7.5). A send is never retried: when Google may have
/// taken the message (the connection failed, a 5xx, or a success answer that could not be
/// read) the composer says to check Sent; a refusal, or a failure before anything went out,
/// carries its reason.
pub(super) fn send_error(e: &CallError) -> SendError {
    let (message, maybe_sent) = match e {
        CallError::Api(
            ApiError::Network(_)
            | ApiError::Http { status: 500.., .. }
            | ApiError::Decode(_)
            | ApiError::TooLarge,
        ) => (MAYBE_SENT.to_string(), true),
        CallError::Api(e) => (e.to_string(), false),
        // The token could not be refreshed, so nothing was sent.
        CallError::Network => (NETWORK_PROBLEM.to_string(), false),
        CallError::Auth | CallError::Gone => (SIGNED_OUT.to_string(), false),
    };
    SendError {
        message,
        maybe_sent,
    }
}

/// The text a failed attachment download shows (a save's toast, a forward's send error).
pub(super) fn attachment_message(e: &CallError) -> String {
    match e {
        CallError::Api(ApiError::TooLarge) => ATTACHMENT_TOO_LARGE.into(),
        CallError::Api(ApiError::NotFound) => ATTACHMENT_GONE.into(),
        e => e.reader_message(),
    }
}

/// A short reason for a failed file write, for the toast.
pub(super) fn io_message(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => "permission denied".into(),
        std::io::ErrorKind::StorageFull => "the disk is full".into(),
        std::io::ErrorKind::NotFound => "the download folder was not found".into(),
        std::io::ErrorKind::ReadOnlyFilesystem => "the disk is read-only".into(),
        kind => kind.to_string(),
    }
}
