mod bot;
mod data_storage;
mod errors;
pub use errors::Error;
mod image_processing;

use data_storage::{Data, PreviouslySeen, SeenImage};

use hyper::Client as HyperClient;
use hyper_rustls::HttpsConnector;

use tokio_stream::StreamExt;

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
    id::{ChannelId, MessageId},
};

#[tokio::main]
async fn main() {
    println!("Booting!");
    dotenv::dotenv().ok();

    let token = std::env::var("DISCORD_TOKEN").expect("no discord token present");

    let web_client =
        HyperClient::builder().build::<_, hyper::Body>(HttpsConnector::with_native_roots());

    let client = Client::builder()
        .hyper_client(web_client.clone())
        .default_allowed_mentions(AllowedMentions::default())
        .token(&token)
        .build();

    println!("Initalizing database...");
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

    println!("Cluster is running...");

    let mut incoming_events = cluster.events();
    while let Some((_shard_id, event)) = incoming_events.next().await {
        context.standby.process(&event);

        // TODO: actually handle MessageUpdate events to catch more images
        let context = context.clone();
        if let Event::MessageCreate(msg) = event {
            tokio::spawn(async move {
                if let Err(e) = handle_message(msg, context).await {
                    eprint!("Error handling a message: {:?}", e);
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
                dispatch_repost_reply(&context, &image, times_seen, message.channel_id).await?;
                return Ok(());
            }
        }
    }

    if message
        .mentions
        .first()
        .filter(|m| context.is_me(m.id))
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
        let image_to_ignore = match if let Some(parent) = &msg.reference {
            // TODO: Run these through a cache
            let msg_with_image = context
                .get_message(
                    parent.channel_id.ok_or(Error::UnsupportedChannelConfig)?,
                    parent.message_id.ok_or(Error::UnsupportedChannelConfig)?,
                )
                .await?;

            match image_from_message(&msg_with_image) {
                Some(url) => Some(context.download_image(url).await?),
                None => None,
            }
        } else {
            match image_from_message(&msg) {
                Some(url) => Some(context.download_image(url).await?),
                None => None,
            }
        } {
            Some(img) => img,
            None => return Ok(()),
        };

        match context
            .confirm_action(bot::ConfirmationAction::IgnoreImage, message.channel_id)
            .await
        {
            Ok(confirmed) => {
                println!("User confirmed: {}", confirmed);

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
) -> Result<(), Error> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clocks are wobbly");
    let difference = now - std::time::Duration::from_secs(previous.sent);

    let hours_ago = difference.as_secs() / 3600;

    let reply = format!("Hey, {} already posted that here {} hours ago. I've seen it {} times now. Try harder next time <:niko:765033287357431829>", 
        previous.author,
        hours_ago,
        times_seen,
    );
    context
        .send_message(
            reply,
            channel_id,
            Some(MessageId(previous.original_message_id)),
        )
        .await?;
    Ok(())
}

fn image_from_message(msg: &Message) -> Option<&str> {
    for embed in &msg.embeds {
        if let Some(img_url) = filter_embed(embed) {
            println!("Embed image found: {:?}", img_url);
            return Some(img_url);
        }
    }

    if let Some(url) = msg.attachments.iter().find_map(|a| filter_image(&a.url)) {
        println!("Image attachment found: {}", url);
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
    println!("Image hash was {:0x?}", hash.as_bytes());

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clocks are wobbly");

    let properties = SeenImage::new(msg.author.name.clone(), now.as_secs(), msg.id.0);
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

    for ext in SUPPORTED_EXTENSIONS.iter().copied() {
        println!("Comparing {} to {}", ext, extension);
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
}
