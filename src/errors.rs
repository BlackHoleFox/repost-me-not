#[derive(Debug)]
pub enum Error {
    Database(DatabaseError),
    InteractionError(DiscordInteractionError),
    DownloadingConent(hyper::Error),
    ContentTooLarge,
    UnsupportedChannelConfig,
    UnsupportedImageFormat(image::error::ImageError),
}

impl From<hyper::Error> for Error {
    fn from(e: hyper::Error) -> Self {
        Self::DownloadingConent(e)
    }
}

impl From<DiscordInteractionError> for Error {
    fn from(e: DiscordInteractionError) -> Self {
        Self::InteractionError(e)
    }
}

impl From<DatabaseError> for Error {
    fn from(e: DatabaseError) -> Self {
        Self::Database(e)
    }
}

#[derive(Debug)]
pub enum DiscordInteractionError {
    SendingMessage(twilight_http::Error),
    FetchingMessage(twilight_http::Error),
    ReactionHandling(twilight_http::Error),
    Deserialize(twilight_http::response::DeserializeBodyError),
    MessageNotFound,
}

#[derive(Debug)]
pub enum DatabaseError {
    Accessing(sled::Error),
    Initalizing(sled::Error),
    Recording(sled::Error),
}
