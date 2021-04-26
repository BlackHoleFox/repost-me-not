use crate::data_storage::Data;
use crate::errors::{DiscordInteractionError, Error};

use hyper::{body::HttpBody, client::HttpConnector, Client as HyperClient, Uri};
use hyper_rustls::HttpsConnector;

use twilight_http::{request::prelude::RequestReactionType, Client};
use twilight_model::{
    channel::{Message, ReactionType},
    gateway::payload::ReactionAdd,
    id::{ChannelId, MessageId, UserId},
};
use twilight_standby::Standby;

use std::{convert::TryInto, str::FromStr, time::Duration};

type WebClient = HyperClient<HttpsConnector<HttpConnector>>;

pub enum ConfirmationAction {
    IgnoreImage,
}

impl ConfirmationAction {
    const CONFIRMED: &'static str = "✅";
    const CANCELED: &'static str = "❌";
    const TIMED_OUT: &'static str = "Waiting period elapsed, moving on";

    const fn as_str(&self) -> &'static str {
        match self {
            Self::IgnoreImage => "Do you want to ignore this image?",
        }
    }
}

#[derive(Clone)] // cheap
pub struct Context {
    pub data: Data,
    web_client: WebClient,
    discord_client: Client,
    pub standby: Standby,
    id: UserId,
}

impl Context {
    pub fn init(me: UserId, data: Data, web_client: WebClient, discord_client: Client) -> Self {
        let standby = Standby::new();

        Self {
            data,
            web_client,
            discord_client,
            standby,
            id: me,
        }
    }

    pub fn is_me(&self, other: UserId) -> bool {
        self.id == other
    }

    pub async fn send_message<M: Into<String>>(
        &self,
        message: M,
        channel: ChannelId,
        reply: Option<MessageId>,
    ) -> Result<Message, DiscordInteractionError> {
        let mut request = self
            .discord_client
            .create_message(channel)
            .content(message)
            .expect("bug: message content was > 2000");

        if let Some(reply_to) = reply {
            request = request.reply(reply_to);
        }

        request
            .await
            .map_err(DiscordInteractionError::SendingMessage)
    }

    pub async fn get_message(
        &self,
        channel: ChannelId,
        message: MessageId,
    ) -> Result<Message, DiscordInteractionError> {
        self.discord_client
            .message(channel, message)
            .await
            .map_err(DiscordInteractionError::FetchingMessage)?
            .ok_or(DiscordInteractionError::MessageNotFound)
    }

    pub async fn confirm_action(
        &self,
        action: ConfirmationAction,
        channel: ChannelId,
    ) -> Result<bool, DiscordInteractionError> {
        let msg = self.send_message(action.as_str(), channel, None).await?;

        let reaction = RequestReactionType::Unicode {
            name: ConfirmationAction::CONFIRMED.to_string(),
        };
        self.discord_client
            .create_reaction(channel, msg.id, reaction)
            .await
            .map_err(DiscordInteractionError::ReactionHandling)?;

        let reaction = RequestReactionType::Unicode {
            name: ConfirmationAction::CANCELED.to_string(),
        };
        self.discord_client
            .create_reaction(channel, msg.id, reaction)
            .await
            .map_err(DiscordInteractionError::ReactionHandling)?;

        let me = self.id;
        let fut = self
            .standby
            .wait_for_reaction(msg.id, move |event: &ReactionAdd| {
                if event.user_id == me {
                    return false;
                }

                check_emote_name_for_confirmation(&event.emoji).is_some()
            });

        match tokio::time::timeout(Duration::from_secs(10), fut).await {
            Ok(Ok(_)) => Ok(true),
            Ok(_) => {
                unreachable!("bug: standby (and context?) was dropped while waiting for reaction")
            }
            Err(_) => {
                self.send_message(ConfirmationAction::TIMED_OUT, channel, None)
                    .await?;

                return Ok(false);
            }
        }
    }

    pub async fn download_image(&self, url: &str) -> Result<Vec<u8>, Error> {
        let uri = Uri::from_str(url).expect("invalid URL");

        let response = self.web_client.get(uri.clone()).await?;
        let size = response
            .size_hint()
            .exact()
            .unwrap_or_else(|| response.size_hint().lower());

        let mut image = Vec::with_capacity(size.try_into().map_err(|_| Error::ContentTooLarge)?);

        let mut body = response.into_body();
        while let Some(bytes) = body.data().await {
            let bytes = bytes?;
            image.extend(bytes);
        }

        Ok(image)
    }
}

fn check_emote_name_for_confirmation(emote: &ReactionType) -> Option<bool> {
    let name = match emote {
        ReactionType::Unicode { name } => name,
        ReactionType::Custom { name: Some(n), .. } => n,
        _ => return None,
    };

    let check = match &**name {
        ConfirmationAction::CONFIRMED => true,
        ConfirmationAction::CANCELED => false,
        name if name.contains("yes") => true,
        name if name.contains("Yes") => true,
        // "no" seems like it would have more false positives with .contains().
        name if name.starts_with("no") => false,
        name if name.starts_with("No") => false,
        _ => return None,
    };

    Some(check)
}

#[cfg(test)]
mod tests {
    use twilight_model::{channel::ReactionType, id::EmojiId};

    use super::*;

    const ACCEPT_AS_YES: &[&str] = &[
        ConfirmationAction::CONFIRMED,
        "yes",
        "Yes",
        "Yesdoit",
        "yesplease",
        "yes_yes_yes",
        "yesmam",
        "yeserp",
        "please_yes_doit",
        "wow_Yes_please",
    ];

    const ACCEPT_AS_NO: &[&str] = &["no", "No", "NoWhywouldIwantthat"];

    const IGNORED_EMOJIS: &[&str] = &["wow_nope", "wow_really_Nope"];

    #[test]
    fn confirmation_emojis_as_yes() {
        for name in ACCEPT_AS_YES {
            let as_unicode = ReactionType::Unicode {
                name: name.to_string(),
            };
            let as_custom = ReactionType::Custom {
                animated: false,
                id: EmojiId(1),
                name: Some(name.to_string()),
            };

            assert_eq!(
                check_emote_name_for_confirmation(&as_unicode),
                Some(true),
                "{} wasn't accepted",
                name
            );
            assert_eq!(
                check_emote_name_for_confirmation(&as_custom),
                Some(true),
                "{} wasn't accepted",
                name
            );
        }
    }

    #[test]
    fn confirmation_emojis_as_no() {
        for name in ACCEPT_AS_NO {
            let as_unicode = ReactionType::Unicode {
                name: name.to_string(),
            };
            let as_custom = ReactionType::Custom {
                animated: true,
                id: EmojiId(1),
                name: Some(name.to_string()),
            };

            assert_eq!(
                check_emote_name_for_confirmation(&as_unicode),
                Some(false),
                "{} wasn't accepted",
                name
            );
            assert_eq!(
                check_emote_name_for_confirmation(&as_custom),
                Some(false),
                "{} wasn't accepted",
                name
            );
        }
    }

    #[test]
    fn confirmation_emojis_ignored() {
        for name in IGNORED_EMOJIS {
            let as_unicode = ReactionType::Unicode {
                name: name.to_string(),
            };
            let as_custom = ReactionType::Custom {
                animated: true,
                id: EmojiId(1),
                name: Some(name.to_string()),
            };

            assert_eq!(
                check_emote_name_for_confirmation(&as_unicode),
                None,
                "{} was wrongly accepted",
                name
            );
            assert_eq!(
                check_emote_name_for_confirmation(&as_custom),
                None,
                "{} was wrongly accepted",
                name
            );
        }

        let with_no_name = ReactionType::Custom {
            animated: false,
            id: EmojiId(1),
            name: None,
        };

        assert_eq!(
            check_emote_name_for_confirmation(&with_no_name),
            None,
            "emote with no name was wrongly accepted"
        );
    }
}
