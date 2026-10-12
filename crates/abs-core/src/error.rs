#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("storage error: {0}")]
    Storage(#[from] abs_storage::StorageError),

    #[error("login failed: {0}")]
    Login(#[from] abs_api::LoginError),

    #[error("the server rejected this session's credentials — sign in again")]
    Auth,

    #[error("server returned an unexpected response: {0}")]
    UnexpectedResponse(String),

    /// The server never answered: the connection was refused, the address didn't resolve or
    /// lead anywhere, or nothing came back in time. Told apart from `UnexpectedResponse` so the
    /// UI can say "can't reach your server" rather than blame the server's answer.
    #[error("couldn't connect to the server: {0}")]
    Unreachable(String),

    /// The app's offline mode is on: nothing may reach the server until it's switched off.
    #[error("offline mode is on")]
    Offline,

    /// The item has no audio this app can play: a podcast (its audio is in episodes) or an
    /// e-book-only book. Retrying can't help.
    #[error("item {0} has no audio files")]
    NoPlayableAudio(String),

    /// The server answered that the item doesn't exist (any more): deleted on the server, or no
    /// longer shared with this account.
    #[error("{0} isn't on the server")]
    NotOnServer(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
