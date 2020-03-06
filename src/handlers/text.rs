use async_trait::async_trait;
use sentry::integrations::failure::capture_fail;
use telegram::*;
use tokio01::runtime::current_thread::block_on_all;

use crate::utils::{get_message, with_user_scope};

pub struct TextHandler;

#[async_trait]
impl crate::Handler for TextHandler {
    fn name(&self) -> &'static str {
        "text"
    }

    async fn handle(
        &self,
        handler: &crate::MessageHandler,
        update: Update,
        _command: Option<Command>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        use quaint::prelude::*;

        let message = match update.message {
            Some(message) => message,
            _ => return Ok(false),
        };

        let text = match &message.text {
            Some(text) => text,
            _ => return Ok(false),
        };

        let now = std::time::Instant::now();

        let from = message.from.clone().unwrap();

        if text.trim().parse::<i32>().is_err() {
            tracing::trace!("got text that wasn't oob, ignoring");
            return Ok(false);
        }

        tracing::trace!("checking if message was Twitter code");

        let conn = handler
            .conn
            .check_out()
            .await
            .expect("Unable to get db conn");
        let result = conn
            .select(
                Select::from_table("twitter_auth")
                    .column("request_key")
                    .column("request_secret")
                    .so_that("user_id".equals(from.id)),
            )
            .await
            .expect("Unable to query db");

        let row = match result.first() {
            Some(row) => row,
            _ => return Ok(true),
        };

        tracing::trace!("we had waiting Twitter code");

        let request_token = egg_mode::KeyPair::new(
            row["request_key"].to_string().unwrap(),
            row["request_secret"].to_string().unwrap(),
        );

        let con_token = egg_mode::KeyPair::new(
            handler.config.twitter_consumer_key.clone(),
            handler.config.twitter_consumer_secret.clone(),
        );

        let token = match block_on_all(egg_mode::access_token(con_token, &request_token, text)) {
            Err(e) => {
                tracing::warn!("user was unable to verify OOB: {:?}", e);

                handler
                    .report_error(
                        &message,
                        Some(vec![("command", "twitter_auth".to_string())]),
                        || capture_fail(&e),
                    )
                    .await;
                return Ok(true);
            }
            Ok(token) => token,
        };

        tracing::trace!("got token");

        let access = match token.0 {
            egg_mode::Token::Access { access, .. } => access,
            _ => unimplemented!(),
        };

        tracing::trace!("got access token");

        conn.delete(Delete::from_table("twitter_account").so_that("user_id".equals(from.id)))
            .await
            .expect("Unable to remove auth keys");

        conn.insert(
            Insert::single_into("twitter_account")
                .value("user_id", from.id)
                .value("consumer_key", access.key.to_string())
                .value("consumer_secret", access.secret.to_string())
                .build(),
        )
        .await
        .expect("Unable to insert credential info");

        conn.delete(Delete::from_table("twitter_auth").so_that("user_id".equals(from.id)))
            .await
            .expect("Unable to remove auth keys");

        let mut args = fluent::FluentArgs::new();
        args.insert("userName", fluent::FluentValue::from(token.2));

        let text = handler
            .get_fluent_bundle(from.language_code.as_deref(), |bundle| {
                get_message(&bundle, "twitter-welcome", Some(args)).unwrap()
            })
            .await;

        let message = SendMessage {
            chat_id: from.id.into(),
            text,
            reply_to_message_id: Some(message.message_id),
            ..Default::default()
        };

        if let Err(e) = handler.bot.make_request(&message).await {
            tracing::warn!("unable to send message: {:?}", e);
            with_user_scope(Some(&from), None, || {
                capture_fail(&e);
            });
        }

        let point = influxdb::Query::write_query(influxdb::Timestamp::Now, "twitter")
            .add_tag("type", "added")
            .add_field("duration", now.elapsed().as_millis() as i64);

        if let Err(e) = handler.influx.query(&point).await {
            tracing::error!("unable to send command to InfluxDB: {:?}", e);
            with_user_scope(Some(&from), None, || {
                capture_fail(&e);
            });
        }

        Ok(true)
    }
}
