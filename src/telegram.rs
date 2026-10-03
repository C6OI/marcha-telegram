use crate::AppState;
use markovchain_client_lib::content_string::ContentString;
use markovchain_client_lib::{GeneratePayload, InputPayload};
use rand::prelude::Distribution;
use ringbuffer::{ConstGenericRingBuffer, RingBuffer};
use std::str::FromStr;
use std::sync::Arc;
use teloxide::adaptors::throttle::Limits;
use teloxide::adaptors::{CacheMe, Throttle};
use teloxide::prelude::*;
use teloxide::sugar::request::RequestReplyExt;
use teloxide::types::{BotCommandScope, ChatAction, Me, MessageEntityKind, MessageEntityRef};
use teloxide::utils::command::BotCommands;
use tokio::sync::Mutex;
use tokio_util::future::FutureExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

type Bot = CacheMe<Throttle<teloxide::Bot>>;

pub struct Telegram {
    pub bot: Bot,
    app_state: Arc<AppState>,
    state: Arc<TelegramState>,
}

struct TelegramState {
    media_group_ids: Mutex<ConstGenericRingBuffer<String, 25>>,
}

#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase", description = "Basic commands")]
enum BasicCommand {
    #[command(description = "Generate a nonsense")]
    Generate(String),
}

#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase", description = "Admin commands")]
enum AdminCommand {
    #[command(description = "Enable the automatic generation in this chat")]
    EnableGen,
    #[command(description = "Disable the automatic generation in this chat")]
    DisableGen,
    #[command(description = "Set random interval between automatic generations in this chat")]
    Interval { interval: i16 },
    #[command(description = "Set random interval between automatic generations in this chat")]
    RandomInterval,
}

impl Telegram {
    #[must_use]
    pub fn new(app_state: Arc<AppState>) -> Self {
        let bot = teloxide::Bot::new(&app_state.settings.telegram.token)
            .throttle(Limits::default())
            .cache_me();

        let state = Arc::new(TelegramState {
            media_group_ids: Mutex::new(ConstGenericRingBuffer::new()),
        });

        Self {
            bot,
            app_state,
            state,
        }
    }

    pub async fn start(self: Arc<Self>, cancellation_token: CancellationToken) {
        register_commands(&self.bot).await;

        let mut dispatcher = Dispatcher::builder(
            self.bot.clone(),
            dptree::entry().branch(
                Update::filter_message()
                    .branch(
                        dptree::filter(|msg: Message| {
                            msg.chat.is_group() || msg.chat.is_supergroup()
                        })
                        .branch(
                            dptree::entry()
                                .filter_mention_command::<AdminCommand>()
                                .endpoint(handle_admin_command),
                        ),
                    )
                    .branch(
                        dptree::entry()
                            .filter_command::<BasicCommand>()
                            .endpoint(handle_basic_command),
                    )
                    .endpoint(message),
            ), /*.branch(Update::filter_guest_message().endpoint(guest_message))*/
        )
        .dependencies(dptree::deps![self.app_state.clone(), self.state.clone()])
        .build();

        let data_about_bot = self.bot.get_me().await.unwrap();
        info!("I am @{}", data_about_bot.username.as_ref().unwrap());
        dispatcher
            .dispatch()
            .with_cancellation_token(&cancellation_token)
            .await;
    }
}

async fn register_commands(bot: &Bot) {
    bot.delete_my_commands()
        .await
        .expect("Failed to delete bot commands");

    bot.set_my_commands(BasicCommand::bot_commands())
        .await
        .expect("Failed to register basic bot commands");

    bot.set_my_commands(
        AdminCommand::bot_commands()
            .into_iter()
            .chain(BasicCommand::bot_commands()),
    )
    .scope(BotCommandScope::AllChatAdministrators)
    .await
    .expect("Failed to register admin bot commands");

    info!("Registered bot commands");
}

async fn guest_message(
    app_state: Arc<AppState>,
    state: Arc<TelegramState>,
    bot: Bot,
    msg: Message,
) -> ResponseResult<()> {
    info!("{msg:?}");
    Ok(())
}

async fn message(
    app_state: Arc<AppState>,
    state: Arc<TelegramState>,
    bot: Bot,
    msg: Message,
) -> ResponseResult<()> {
    let Some(from) = &msg.from else {
        return Ok(());
    };

    debug!(
        "Message from {}",
        from.username.as_ref().unwrap_or(&from.first_name)
    );

    // Save the input only from normal users
    if !from.is_bot
        && let Some(text) = msg.text().or_else(|| msg.caption())
    {
        save_input(&app_state, text).await;
    }

    // Save input from all messages, but answer only once in case of media group
    if let Some(media_group_id) = &msg.media_group_id() {
        let mut media_group_ids = state.media_group_ids.lock().await;

        if media_group_ids.contains(&media_group_id.0) {
            return Ok(());
        }

        media_group_ids.enqueue(media_group_id.0.clone());
    }

    // Prevent possible mention loop, answer only to other users
    if from.id != bot.get_me().await?.id {
        answer_to_mention(&app_state, &bot, &msg).await?;
    }

    // Try to generate the text if auto generation is enabled in the chat
    generate_from_interval(&app_state, &bot, &msg).await?;

    Ok(())
}

async fn handle_basic_command(
    app_state: Arc<AppState>,
    bot: Bot,
    msg: Message,
    cmd: BasicCommand,
) -> ResponseResult<()> {
    bot.send_chat_action(msg.chat.id, ChatAction::Typing)
        .await?;

    match cmd {
        BasicCommand::Generate(text) => {
            let mut gen_text = Some(text.as_str());
            let mut max_length = None;

            if let Some((prefix, suffix)) = text.rsplit_once(' ') {
                gen_text = Some(suffix);

                if let Ok(max_len) = usize::from_str(suffix) {
                    gen_text = Some(prefix);
                    max_length = Some(max_len);
                }
            }

            let text = gen_text.and_then(|s| ContentString::try_from(s).ok());
            let text = match generate(&app_state, text, max_length).await {
                Ok(text) => text,
                Err(err) => {
                    warn!("Failed to generate a text for command: {err}");
                    return Ok(());
                }
            };

            bot.send_message(msg.chat.id, text).reply_to(msg.id).await?;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn handle_admin_command(
    app_state: Arc<AppState>,
    bot: Bot,
    msg: Message,
    cmd: AdminCommand,
) -> ResponseResult<()> {
    let Some(from) = msg.from else {
        return Ok(());
    };

    let admins = bot.get_chat_administrators(msg.chat.id).await?;
    if admins.iter().all(|admin| admin.user.id != from.id) {
        return Ok(());
    }

    bot.send_chat_action(msg.chat.id, ChatAction::Typing)
        .await?;

    match cmd {
        AdminCommand::EnableGen => {
            let db = app_state
                .db_pool
                .get()
                .await
                .expect("Failed to get the db client from the pool");

            let statement = db
                .prepare_cached(
                    /* language=postgresql */
                    r"
                SELECT EXISTS (
                    SELECT 1
                    FROM enabled_chats
                    WHERE chat_id = $1
                );
            ",
                )
                .await
                .expect("Failed to prepare the exists query");

            let chat_id = msg.chat.id.0;

            let exists = db
                .query_one(&statement, &[&chat_id])
                .await
                .expect("Failed to execute the exists query")
                .get(0);

            if exists {
                bot.send_message(
                    msg.chat.id,
                    "Automatic generation is already enabled in this chat",
                )
                .reply_to(msg.id)
                .await?;
                return Ok(());
            }

            let statement = db
                .prepare_cached(
                    /* language=postgresql */
                    r"
                INSERT INTO enabled_chats (chat_id, msgs_until_gen)
                VALUES ($1, $2);
            ",
                )
                .await
                .expect("Failed to prepare the insert query");

            let messages_until_generation = i16::from(app_state.uniform.sample(&mut rand::rng()));
            db.execute(&statement, &[&chat_id, &messages_until_generation])
                .await
                .expect("Failed to execute the insert query");

            bot.send_message(msg.chat.id, "Enabled the automatic generation in this chat")
                .reply_to(msg.id)
                .await?;
        }
        AdminCommand::DisableGen => {
            let db = app_state
                .db_pool
                .get()
                .await
                .expect("Failed to get the db client from the pool");

            let chat_id = msg.chat.id.0;

            let statement = db
                .prepare_cached(
                    /* language=postgresql */
                    r"
                DELETE FROM enabled_chats
                WHERE chat_id = $1
            ",
                )
                .await
                .expect("Failed to prepare the delete query");

            let disabled = db
                .execute(&statement, &[&chat_id])
                .await
                .expect("Failed to execute the delete query")
                > 0;

            if disabled {
                bot.send_message(
                    msg.chat.id,
                    "Disabled the automatic generation in this chat",
                )
                .reply_to(msg.id)
                .await?;
            } else {
                bot.send_message(
                    msg.chat.id,
                    "Automatic generation is already disabled in this chat",
                )
                .reply_to(msg.id)
                .await?;
            }
        }
        AdminCommand::Interval { interval } => {
            let db = app_state
                .db_pool
                .get()
                .await
                .expect("Failed to get the db client from the pool");

            let statement = db
                .prepare_cached(
                    /* language=postgresql */
                    r"
                UPDATE enabled_chats
                SET interval = $2,
                    msgs_until_gen = least($2, msgs_until_gen)
                WHERE chat_id = $1
            ",
                )
                .await
                .expect("Failed to prepare the update query");

            let chat_id = msg.chat.id.0;

            let updated = db
                .execute(&statement, &[&chat_id, &interval])
                .await
                .expect("Failed to execute the update query")
                > 0;

            if updated {
                bot.send_message(msg.chat.id, format!("The interval between automatic generations is set to {interval} in this chat")).reply_to(msg.id).await?;
            } else {
                bot.send_message(
                    msg.chat.id,
                    "Cannot change interval because automatic generation is disabled in this chat",
                )
                .reply_to(msg.id)
                .await?;
            }
        }
        AdminCommand::RandomInterval => {
            let db = app_state
                .db_pool
                .get()
                .await
                .expect("Failed to get the db client from the pool");

            let statement = db
                .prepare_cached(
                    /* language=postgresql */
                    r"
                UPDATE enabled_chats
                SET interval = $2,
                    msgs_until_gen = least($2, msgs_until_gen)
                WHERE chat_id = $1
            ",
                )
                .await
                .expect("Failed to prepare the update query");

            let chat_id = msg.chat.id.0;

            let updated = db
                .execute(&statement, &[&chat_id, &Option::<i16>::None])
                .await
                .expect("Failed to execute the update query")
                > 0;

            if updated {
                bot.send_message(
                    msg.chat.id,
                    "The interval between automatic generations is set to random in this chat",
                )
                .reply_to(msg.id)
                .await?;
            } else {
                bot.send_message(
                    msg.chat.id,
                    "Cannot change interval because automatic generation is disabled in this chat",
                )
                .reply_to(msg.id)
                .await?;
            }
        }
    }

    Ok(())
}

async fn save_input(app_state: &Arc<AppState>, input: &str) {
    let mut join_set = tokio::task::JoinSet::new();

    for chunk in input.chars().collect::<Vec<_>>().chunks(2000) {
        let chunk_str = chunk.iter().collect::<String>();

        let Ok(input) = ContentString::try_from(chunk_str) else {
            return;
        };

        let payload = InputPayload { input };

        let app_state = app_state.clone();

        join_set.spawn(async move {
            if let Err(err) = app_state.markov_chain.input(payload).await {
                warn!("Failed to execute `input`: {err}");
            }
        });
    }

    join_set.join_all().await;
}

async fn answer_to_mention(
    app_state: &Arc<AppState>,
    bot: &Bot,
    msg: &Message,
) -> ResponseResult<()> {
    if !mentions_me(bot, msg).await? {
        return Ok(());
    }

    bot.send_chat_action(msg.chat.id, ChatAction::Typing)
        .await?;

    let text = msg.text().or_else(|| msg.caption()).unwrap_or("");
    let text = match continue_message(app_state, text).await {
        Ok(text) => text,
        Err(err) => {
            warn!("Failed to generate a text for mention: {err}");
            return Ok(());
        }
    };

    bot.send_message(msg.chat.id, text).reply_to(msg.id).await?;
    Ok(())
}

async fn mentions_me(bot: &Bot, msg: &Message) -> ResponseResult<bool> {
    if msg.chat.is_private() {
        return Ok(true);
    }

    let me = bot.get_me().await?;

    if msg.mentioned_users().any(|user| user.id == me.id) {
        return Ok(true);
    }

    if let Some(text) = msg.text()
        && let Some(entities) = msg.parse_entities()
        && find_mention(entities, text, &me)
    {
        return Ok(true);
    }

    if let Some(caption) = msg.caption()
        && let Some(entities) = msg.parse_caption_entities()
        && find_mention(entities, caption, &me)
    {
        return Ok(true);
    }

    Ok(false)
}

fn find_mention(entities: Vec<MessageEntityRef>, text: &str, me: &Me) -> bool {
    for entity in entities {
        match entity.kind() {
            MessageEntityKind::Mention => {
                if let Some(mention) = text.get(entity.range())
                    && mention == format!("@{}", me.username())
                {
                    return true;
                }
            }
            MessageEntityKind::TextMention { user } if user.id == me.id => {
                return true;
            }
            _ => {}
        }
    }

    false
}

async fn generate_from_interval(
    app_state: &AppState,
    bot: &Bot,
    msg: &Message,
) -> ResponseResult<()> {
    let chat_id = msg.chat.id.0;

    let should_generate = update_gen_interval(app_state, chat_id)
        .await
        .expect("Failed to update generation interval");

    if !should_generate {
        return Ok(());
    }

    bot.send_chat_action(msg.chat.id, ChatAction::Typing)
        .await?;

    let text = msg.text().or_else(|| msg.caption()).unwrap_or("");
    let text = match continue_message(app_state, text).await {
        Ok(text) => text,
        Err(err) => {
            warn!("Failed to generate a text: {err}");
            return Ok(());
        }
    };

    bot.send_message(msg.chat.id, text).await?;
    Ok(())
}

async fn update_gen_interval(app_state: &AppState, chat_id: i64) -> anyhow::Result<bool> {
    let client = app_state
        .db_pool
        .get()
        .await
        .expect("Failed to get the db client from the pool");

    let statement = client
        .prepare_cached(
            /* language=postgresql */
            r"
        UPDATE enabled_chats
        SET msgs_until_gen = msgs_until_gen - 1
        WHERE chat_id = $1
        RETURNING msgs_until_gen, interval;
    ",
        )
        .await?;

    let Some(row) = client.query_opt(&statement, &[&chat_id]).await? else {
        return Ok(false);
    };

    let count: i16 = row.get("msgs_until_gen");

    // reset the `msgs_until_gen` if it is <= 0
    if count <= 0 {
        let interval: Option<i16> = row.get("interval");
        let new_count =
            interval.unwrap_or_else(|| i16::from(app_state.uniform.sample(&mut rand::rng())));

        let statement = client
            .prepare_cached(
                /* language=postgresql */
                r"
            UPDATE enabled_chats
            SET msgs_until_gen = $2
            WHERE chat_id = $1;
        ",
            )
            .await?;

        client.execute(&statement, &[&chat_id, &new_count]).await?;
    }

    Ok(count == 0)
}

async fn continue_message(app_state: &AppState, text: &str) -> anyhow::Result<String> {
    let start = text
        .rsplit(' ')
        .next()
        .and_then(|s| ContentString::try_from(s).ok());

    let mut text = generate(app_state, start, None)
        .await
        .map(|text| text.split_once(' ').unwrap_or((text.as_str(), "")).1.into());

    if text.as_ref().is_ok_and(|text: &String| text.is_empty()) {
        text = generate(app_state, None, None).await;
    }

    text
}

async fn generate(
    app_state: &AppState,
    start: Option<ContentString>,
    max_length: Option<usize>,
) -> anyhow::Result<String> {
    let payload = GeneratePayload { start, max_length };
    let text = app_state.markov_chain.generate(payload).await?;
    Ok(text)
}
