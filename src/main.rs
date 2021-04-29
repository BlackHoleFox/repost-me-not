mod bot;
mod data_storage;
mod errors;
use std::borrow::Cow;

pub use errors::Error;
mod image_processing;

use data_storage::{Data, PreviouslySeen, SeenImage};

use hyper::Client as HyperClient;
use hyper_rustls::HttpsConnector;

use tokio_stream::StreamExt;

use tracing_subscriber::{EnvFilter, FmtSubscriber};
use twilight_gateway::{
    cluster::{Cluster, ShardScheme},
    Event,
};
use twilight_http::{request::channel::allowed_mentions::AllowedMentions, Client};
use twilight_model::{
    channel::{
        embed::{Embed, EmbedImage},
        message::Message,
    },
    gateway::{payload::MessageCreate, Intents},
    id::{ChannelId, GuildId, MessageId},
};

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();

    tracing::subscriber::set_global_default(
        FmtSubscriber::builder()
            .with_env_filter(EnvFilter::from_default_env())
            .finish(),
    )
    .unwrap();

    tracing::info!("Booting!");

    let token = std::env::var("DISCORD_TOKEN").expect("no discord token present");

    let web_client =
        HyperClient::builder().build::<_, hyper::Body>(HttpsConnector::with_native_roots());

    let client = Client::builder()
        .hyper_client(web_client.clone())
        .default_allowed_mentions(AllowedMentions::default())
        .token(&token)
        .build();

    tracing::info!("Initalizing database...");
    let data = Data::init("./storage").unwrap();

    let me = client.current_user().await.unwrap();

    let context = bot::Context::init(me.id, data, web_client, client);

    let cluster = Cluster::builder(
        token,
        Intents::GUILD_MESSAGES | Intents::GUILD_MESSAGE_REACTIONS,
    )
    .shard_scheme(ShardScheme::Auto)
    .build()
    .await
    .expect("failed to init cluster");

    let spawner = cluster.clone();
    tokio::spawn(async move {
        spawner.up().await;
    });

    tracing::info!("Cluster is running...");

    let mut incoming_events = cluster.events();
    while let Some((_shard_id, event)) = incoming_events.next().await {
        context.standby.process(&event);

        // TODO: actually handle MessageUpdate events to catch more images
        let context = context.clone();
        if let Event::MessageCreate(msg) = event {
            // Maybe someone has an image bot! Imagine that.
            if msg.author.bot {
                continue;
            }

            tokio::spawn(async move {
                if let Err(e) = handle_message(msg, context).await {
                    tracing::error!("Error handling a message: {:?}", e);
                }
            });
        }
    }
}

async fn handle_message(message: Box<MessageCreate>, context: bot::Context) -> Result<(), Error> {
    if let Some(url) = image_from_message(&message) {
        let image = context.download_image(url).await?;
        if let PreviouslySeen::Yes { image, times_seen } = save_image(&context, image, &message)? {
            if !image.ignored {
                dispatch_repost_reply(
                    &context,
                    &image,
                    times_seen,
                    message.channel_id,
                    message.guild_id.ok_or(Error::UnsupportedChannelConfig)?,
                )
                .await?;
                return Ok(());
            }
        }
    }

    if message
        .mentions
        .first()
        .or_else(|| message.mentions.get(1))
        .filter(|m| context.is_me(m.id))
        .is_none()
    {
        return Ok(());
    }

    if let Some(msg) = &message.referenced_message {
        if !message.content.contains("ignore") {
            return Ok(());
        }

        // Support two behaviors for ignoring stuff:
        // 1. Reply on the message containing the image itself
        // 2. Reply to our reply notifying users of a repost.

        let msg_with_img = if let Some(parent) = &msg.reference {
            // TODO: Run these through a cache
            Cow::Owned(
                context
                    .get_message(
                        parent.channel_id.ok_or(Error::UnsupportedChannelConfig)?,
                        parent.message_id.ok_or(Error::UnsupportedChannelConfig)?,
                    )
                    .await?,
            )
        } else {
            Cow::Borrowed(&**message)
        };

        let image_to_ignore = match image_from_message(&msg_with_img) {
            Some(url) => context.download_image(url).await?,
            None => return Ok(()),
        };

        match context
            .confirm_action(bot::ConfirmationAction::IgnoreImage, message.channel_id)
            .await
        {
            Ok(confirmed) => {
                tracing::debug!("User confirmed: {}", confirmed);

                if confirmed {
                    let image_hash = image_processing::process_image(image_to_ignore)?;
                    context.data.access_image(image_hash.as_bytes(), |seen| {
                        // SAFETY: No data is moved out of `seen`, only written to.
                        *(unsafe { &mut seen.get_unchecked_mut().ignored }) = true;
                        true
                    })?;
                }

                context
                    .send_message(
                        format!("User responded with {}", confirmed),
                        message.channel_id,
                        None,
                    )
                    .await
                    .unwrap();
            }
            Err(e) => return Err(e.into()),
        }
    }

    Ok(())
}

async fn dispatch_repost_reply(
    context: &bot::Context,
    previous: &SeenImage,
    times_seen: u64,
    channel_id: ChannelId,
    guild_id: GuildId,
) -> Result<(), Error> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clocks are wobbly");
    let difference = now - std::time::Duration::from_secs(previous.sent);

    let since = time_since(difference.as_secs());

    let message = format!(
        "Hey, {} already posted that here {}. I've seen it {} times now. Try harder next time <:niko:765033287357431829>", 
        previous.author,
        since,
        times_seen
    );

    // Check if we can use replies.
    if channel_id.0 == previous.channel_id {
        context
            .send_message(
                message,
                channel_id,
                Some(MessageId(previous.original_message_id)),
            )
            .await?;
    } else {
        let jump_link = format!(
            "[Jump Link](https://discordapp.com/channels/{}/{}/{})",
            guild_id.0, previous.channel_id, previous.original_message_id
        );
        context.send_embed(message, jump_link, channel_id).await?;
    }

    Ok(())
}

fn time_since(seconds: u64) -> String {
    let days = seconds / 86400;

    if days > 0 {
        let unit = if days > 1 { "days" } else { "day" };

        return format!("{} {} ago", days, unit);
    }

    let hours = seconds / 3600;

    if hours > 0 {
        let unit = if hours > 1 { "hours" } else { "hour" };

        return format!("{} {} ago", hours, unit);
    }

    let minutes = seconds / 60;

    if minutes > 0 {
        let unit = if minutes > 1 { "minutes" } else { "minute" };

        return format!("{} {} ago", minutes, unit);
    }

    let unit = if seconds > 1 { "seconds" } else { "second" };

    format!("{} {} ago", seconds, unit)
}

fn image_from_message(msg: &Message) -> Option<&str> {
    for embed in &msg.embeds {
        if let Some(img_url) = filter_embed(embed) {
            tracing::debug!("Embed image found: {:?}", img_url);
            return Some(img_url);
        }
    }

    if let Some(url) = msg.attachments.iter().find_map(|a| filter_image(&a.url)) {
        tracing::debug!("Image attachment found: {}", url);
        return Some(url);
    }

    None
}

fn save_image(
    context: &bot::Context,
    image: Vec<u8>,
    msg: &Message,
) -> Result<PreviouslySeen, Error> {
    let hash = image_processing::process_image(image)?;
    tracing::debug!("Image hash was {:0x?}", hash.as_bytes());

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clocks are wobbly");

    let properties = SeenImage::new(
        msg.author.name.clone(),
        now.as_secs(),
        msg.id.0,
        msg.channel_id.0,
    );
    let existing = context.data.record_image(&hash, properties)?;
    Ok(existing)
}

fn filter_embed(embed: &Embed) -> Option<&str> {
    let url = match (embed.kind.as_str(), &embed.url, &embed.image) {
        ("image", Some(url), _) => url,
        (_, _, Some(EmbedImage { url: Some(url), .. })) => url,
        _ => return None,
    };

    filter_image(url)
}

const SUPPORTED_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];
const EXTENSION_CLEANUP: &[char] = &[':'];

fn filter_image(url: &str) -> Option<&str> {
    let mut extension = url.split('.').last()?;
    for to_clean in EXTENSION_CLEANUP {
        extension = extension.split(*to_clean).next()?;
    }

    for ext in SUPPORTED_EXTENSIONS.iter() {
        if ext.eq_ignore_ascii_case(extension) {
            return Some(url);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use twilight_model::{
        channel::{message::MessageType, Attachment},
        id::{AttachmentId, ChannelId, GuildId, UserId},
        user::User,
    };

    const SHOULD_BE_PARSED: &[&str] = &[
        "https://pbs.twimg.com/media/EwVMLkNVcAEPZ3i.jpg:large",
        "https://cdn.discordapp.com/attachments/711272231296303236/820868963335405619/lmao.png",
        "https://cdn.discordapp.com/attachments/711272231296303236/835723010555510855/rinshock.PNG",
    ];

    const USER: User = User {
        avatar: None,
        bot: false,
        discriminator: String::new(),
        email: None,
        flags: None,
        id: UserId(0),
        locale: None,
        mfa_enabled: None,
        name: String::new(),
        premium_type: None,
        public_flags: None,
        system: None,
        verified: None,
    };

    #[test]
    fn url_cleanup() {
        for url in SHOULD_BE_PARSED {
            assert!(filter_image(*url).is_some())
        }
    }

    fn msg() -> Message {
        Message {
            activity: None,
            application: None,
            attachments: Vec::new(),
            author: USER,
            channel_id: ChannelId(0),
            content: "wow, much image, very pixels".to_string(),
            edited_timestamp: None,
            embeds: Vec::new(),
            flags: None,
            guild_id: Some(GuildId(0)),
            id: MessageId(0),
            kind: MessageType::Regular,
            member: None,
            mention_channels: Vec::new(),
            mention_everyone: false,
            mention_roles: Vec::new(),
            mentions: Vec::new(),
            pinned: false,
            reactions: Vec::new(),
            reference: None,
            referenced_message: None,
            stickers: Vec::new(),
            timestamp: String::new(),
            tts: false,
            webhook_id: None,
        }
    }

    fn embed() -> Embed {
        Embed {
            author: None,
            color: None,
            description: None,
            fields: Vec::new(),
            footer: None,
            image: None,
            kind: "image".to_string(),
            provider: None,
            thumbnail: None,
            timestamp: None,
            title: None,
            url: None,
            video: None,
        }
    }

    #[test]
    fn message_image_extraction() {
        let mut with_embed_only_url = msg();
        let mut url_embed = embed();
        url_embed.url = Some(SHOULD_BE_PARSED[0].to_string());
        with_embed_only_url.embeds = vec![url_embed];

        let mut with_embed_image = msg();
        let mut image_embed = embed();
        image_embed.image = Some(EmbedImage {
            height: None,
            proxy_url: None,
            url: Some(SHOULD_BE_PARSED[0].to_string()),
            width: None,
        });
        with_embed_image.embeds = vec![image_embed];

        let mut upload_attachment = msg();
        upload_attachment.attachments = vec![Attachment {
            content_type: None,
            filename: "wow.png".to_string(),
            height: None,
            id: AttachmentId(0),
            proxy_url: String::new(),
            size: 483843,
            url: SHOULD_BE_PARSED[1].to_string(),
            width: None,
        }];

        let cases = &[with_embed_only_url, with_embed_image, upload_attachment];

        for msg in cases {
            assert!(image_from_message(msg).is_some())
        }
    }

    const TIME_SINCE_CASES: &[(u64, &str)] = &[
        (24, "seconds"),
        (1, "second"),
        (60, "minute"),
        (322, "minutes"),
        (3900, "hour"),
        (8000, "hours"),
        (86454, "day"),
        (20030303, "days"),
    ];

    #[test]
    fn time_since_messages() {
        for (case, expected) in TIME_SINCE_CASES {
            assert_eq!(
                time_since(*case).split_whitespace().nth(1).unwrap(),
                *expected
            )
        }
    }
}
