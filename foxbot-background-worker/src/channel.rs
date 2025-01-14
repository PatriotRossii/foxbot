use std::sync::Arc;

use crate::*;

#[tracing::instrument(skip(handler, job), fields(job_id = job.id()))]
#[deny(clippy::unwrap_used)]
pub async fn process_channel_update(handler: Arc<Handler>, job: faktory::Job) -> Result<(), Error> {
    let data = job
        .args()
        .iter()
        .next()
        .ok_or(Error::MissingData)?
        .to_owned();
    let message: tgbotapi::Message = serde_json::value::from_value(data)?;

    tracing::trace!("got enqueued message: {:?}", message);

    // Photos should exist for job to be enqueued.
    let sizes = match &message.photo {
        Some(photo) => photo,
        _ => return Ok(()),
    };

    let file = find_best_photo(sizes).ok_or(Error::MissingData)?;
    let (searched_hash, mut matches) = match_image(
        &handler.telegram,
        &handler.conn,
        &handler.fuzzysearch,
        file,
        Some(3),
    )
    .await?;

    // Only keep matches with a distance of 3 or less
    matches.retain(|m| m.distance.unwrap_or(10) <= 3);

    if matches.is_empty() {
        tracing::debug!("unable to find sources for image");
        return Ok(());
    }

    let links = extract_links(&message);

    let mut sites = handler.sites.lock().await;

    // If any matches contained a link we found in the message, skip adding
    // a source.
    if matches
        .iter()
        .any(|file| link_was_seen(&sites, &links, &file.url()))
    {
        tracing::trace!("post already contained valid source url");
        return Ok(());
    }

    if !links.is_empty() {
        let mut results: Vec<foxbot_sites::PostInfo> = Vec::new();
        let _ = find_images(&tgbotapi::User::default(), links, &mut sites, &mut |info| {
            results.extend(info.results);
        })
        .await;

        let urls: Vec<_> = results
            .iter()
            .map::<&str, _>(|result| &result.url)
            .collect();
        if has_similar_hash(searched_hash, &urls).await {
            tracing::debug!("url in post contained similar hash");

            return Ok(());
        }
    }

    drop(sites);

    if already_had_source(&handler.redis, &message, &matches).await? {
        tracing::trace!("post group already contained source url");
        return Ok(());
    }

    // Keep order of sites consistent.
    sort_results_by(&foxbot_models::Sites::default_order(), &mut matches, true);

    let firsts = first_of_each_site(&matches)
        .into_iter()
        .map(|(site, file)| (site, file.url()))
        .collect();

    let data = serde_json::to_value(&MessageEdit {
        chat_id: message.chat.id.to_string(),
        message_id: message.message_id,
        media_group_id: message.media_group_id,
        firsts,
    })?;

    let mut job = faktory::Job::new("channel_edit", vec![data]).on_queue("foxbot_background");
    job.custom = get_faktory_custom();

    handler.enqueue(job).await;

    Ok(())
}

#[tracing::instrument(skip(handler, job), fields(job_id = job.id()))]
#[deny(clippy::unwrap_used)]
pub async fn process_channel_edit(handler: Arc<Handler>, job: faktory::Job) -> Result<(), Error> {
    let data: serde_json::Value = job
        .args()
        .iter()
        .next()
        .ok_or(Error::MissingData)?
        .to_owned();

    tracing::trace!("got enqueued edit: {:?}", data);

    let MessageEdit {
        chat_id,
        message_id,
        media_group_id,
        firsts,
    } = serde_json::value::from_value(data.clone())?;
    let chat_id: &str = &chat_id;

    if let Some(at) = check_more_time(&handler.redis, chat_id).await {
        tracing::trace!("need to wait more time for this chat: {}", at);

        let mut job = faktory::Job::new("channel_edit", vec![data]).on_queue("foxbot_background");
        job.at = Some(at);
        job.custom = get_faktory_custom();

        handler.enqueue(job).await;

        return Ok(());
    }

    // If this photo was part of a media group, we should set a caption on
    // the image because we can't make an inline keyboard on it.
    let resp = if media_group_id.is_some() {
        let caption = firsts
            .into_iter()
            .map(|(_site, url)| url)
            .collect::<Vec<_>>()
            .join("\n");

        let edit_caption_markup = EditMessageCaption {
            chat_id: chat_id.into(),
            message_id: Some(message_id),
            caption: Some(caption),
            ..Default::default()
        };

        handler.telegram.make_request(&edit_caption_markup).await
    // Not a media group, we should create an inline keyboard.
    } else {
        let buttons: Vec<_> = firsts
            .into_iter()
            .map(|(site, url)| InlineKeyboardButton {
                text: site.as_str().to_string(),
                url: Some(url),
                ..Default::default()
            })
            .collect();

        let buttons = if buttons.len() % 2 == 0 {
            buttons.chunks(2).map(|chunk| chunk.to_vec()).collect()
        } else {
            buttons.chunks(1).map(|chunk| chunk.to_vec()).collect()
        };

        let markup = InlineKeyboardMarkup {
            inline_keyboard: buttons,
        };

        let edit_reply_markup = EditMessageReplyMarkup {
            chat_id: chat_id.into(),
            message_id: Some(message_id),
            reply_markup: Some(ReplyMarkup::InlineKeyboardMarkup(markup)),
            ..Default::default()
        };

        handler.telegram.make_request(&edit_reply_markup).await
    };

    match resp {
        // When we get rate limited, mark the job as successful and enqueue
        // it again after the retry after period.
        Err(tgbotapi::Error::Telegram(tgbotapi::TelegramError {
            parameters:
                Some(tgbotapi::ResponseParameters {
                    retry_after: Some(retry_after),
                    ..
                }),
            ..
        })) => {
            tracing::warn!(retry_after, "rate limiting, re-enqueuing");

            let now = chrono::offset::Utc::now();
            let retry_at = now.add(chrono::Duration::seconds(retry_after as i64));

            needs_more_time(&handler.redis, chat_id, retry_at).await;

            let mut job =
                faktory::Job::new("channel_edit", vec![data]).on_queue("foxbot_background");
            job.at = Some(retry_at);
            job.custom = get_faktory_custom();

            handler.enqueue(job).await;

            Ok(())
        }
        // It seems like the bot gets updates from channels, but is unable
        // to update them. Telegram often gives us a
        // 'Bad Request: MESSAGE_ID_INVALID' response here.
        //
        // The corresponding updates have inline keyboard markup, suggesting
        // that they were generated by a bot.
        //
        // I'm not sure if there's any way to detect this before processing
        // an update, so ignore these errors.
        Err(tgbotapi::Error::Telegram(tgbotapi::TelegramError {
            error_code: Some(400),
            description,
            ..
        })) => {
            tracing::warn!("got 400 error, ignoring: {:?}", description);

            Ok(())
        }
        // If permissions have changed (bot was removed from channel, etc.)
        // we may no longer be allowed to process this update. There's
        // nothing else we can do so mark it as successful.
        Err(tgbotapi::Error::Telegram(tgbotapi::TelegramError {
            error_code: Some(403),
            description,
            ..
        })) => {
            tracing::warn!("got 403 error, ignoring: {:?}", description);

            Ok(())
        }
        Ok(_) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Telegram only shows a caption on a media group if there is a single caption
/// anywhere in the group. When users upload a group, we need to check if we can
/// only set a single source to make the link more visible. This can be done by
/// ensuring our source has been previouly used in the media group.
///
/// For our implementation, this is done by maintaining a Redis set of every
/// source previously displayed. If adding our source links returns fewer
/// inserted than we had, it means a link was previously used and therefore we
/// do not have to set a source.
///
/// Because Telegram doesn't send media groups at once, we have to store these
/// values until we're sure the group is over. In this case, we will store
/// values for 300 seconds.
///
/// No link normalization is required here because all links are already
/// normalized when coming from FuzzySearch.
async fn already_had_source(
    conn: &redis::aio::ConnectionManager,
    message: &tgbotapi::Message,
    matches: &[fuzzysearch::File],
) -> anyhow::Result<bool> {
    use redis::AsyncCommands;

    let group_id = match &message.media_group_id {
        Some(id) => id,
        _ => return Ok(false),
    };

    let key = format!("group-sources:{}", group_id);

    let mut urls: Vec<_> = matches.iter().map(|m| m.url()).collect();
    urls.sort();
    urls.dedup();
    let source_count = urls.len();

    tracing::trace!(%group_id, "adding new sources: {:?}", urls);

    let mut conn = conn.clone();
    let added_links: usize = conn.sadd(&key, urls).await?;
    conn.expire(&key, 300).await?;

    tracing::debug!(
        source_count,
        added_links,
        "determined existing and new source links"
    );

    Ok(source_count > added_links)
}

/// Check if any of the provided image URLs have a hash similar to the given
/// input.
#[tracing::instrument(skip(urls))]
async fn has_similar_hash(to: i64, urls: &[&str]) -> bool {
    let to = to.to_be_bytes();

    for url in urls {
        let check_size = CheckFileSize::new(url, 50_000_000);
        let bytes = match check_size.into_bytes().await {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!("unable to download image: {:?}", err);

                continue;
            }
        };

        let hash = tokio::task::spawn_blocking(move || {
            use std::convert::TryInto;

            let hasher = fuzzysearch::get_hasher();

            let im = match image::load_from_memory(&bytes) {
                Ok(im) => im,
                Err(err) => {
                    tracing::warn!("unable to load image: {:?}", err);

                    return None;
                }
            };

            let hash = hasher.hash_image(&im);
            let bytes: [u8; 8] = hash.as_bytes().try_into().unwrap_or_default();

            Some(bytes)
        })
        .in_current_span()
        .await
        .unwrap_or_default();

        let hash = match hash {
            Some(hash) => hash,
            _ => continue,
        };

        if hamming::distance_fast(&to, &hash).unwrap() <= 3 {
            tracing::debug!(url, hash = i64::from_be_bytes(hash), "hashes were similar");

            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    async fn get_redis() -> redis::aio::ConnectionManager {
        let redis_client =
            redis::Client::open(std::env::var("REDIS_DSN").expect("Missing REDIS_DSN")).unwrap();
        redis::aio::ConnectionManager::new(redis_client)
            .await
            .expect("unable to open Redis connection")
    }

    #[tokio::test]
    #[ignore]
    async fn test_already_had_source() {
        let _ = tracing_subscriber::fmt::try_init();

        use super::already_had_source;

        let mut conn = get_redis().await;
        let _: () = redis::cmd("FLUSHDB").query_async(&mut conn).await.unwrap();

        let site_info = Some(fuzzysearch::SiteInfo::FurAffinity(
            fuzzysearch::FurAffinityFile { file_id: 123 },
        ));

        let message = tgbotapi::Message {
            media_group_id: Some("test-group".to_string()),
            ..Default::default()
        };

        let sources = vec![fuzzysearch::File {
            site_id: 123,
            site_info: site_info.clone(),
            ..Default::default()
        }];

        let resp = already_had_source(&conn, &message, &sources).await;
        assert!(resp.is_ok(), "filtering should not cause an error");
        assert_eq!(
            resp.unwrap(),
            false,
            "filtering with no results should have no status flag"
        );

        let resp = already_had_source(&conn, &message, &sources).await;
        assert!(resp.is_ok(), "filtering should not cause an error");
        assert_eq!(
            resp.unwrap(),
            true,
            "filtering with same media group id and source should flag completed"
        );

        let sources = vec![fuzzysearch::File {
            site_id: 456,
            site_info: site_info.clone(),
            ..Default::default()
        }];

        let resp = already_had_source(&conn, &message, &sources).await;
        assert!(resp.is_ok(), "filtering should not cause an error");
        assert_eq!(
            resp.unwrap(),
            false,
            "filtering same group with new source should have no status flag"
        );

        let message = tgbotapi::Message {
            media_group_id: Some("test-group-2".to_string()),
            ..Default::default()
        };

        let resp = already_had_source(&conn, &message, &sources).await;
        assert!(resp.is_ok(), "filtering should not cause an error");
        assert_eq!(
            resp.unwrap(),
            false,
            "different group should not be affected by other group sources"
        );

        let sources = vec![
            fuzzysearch::File {
                site_id: 456,
                site_info: site_info.clone(),
                ..Default::default()
            },
            fuzzysearch::File {
                site_id: 789,
                site_info: site_info.clone(),
                ..Default::default()
            },
        ];

        let resp = already_had_source(&conn, &message, &sources).await;
        assert!(resp.is_ok(), "filtering should not cause an error");
        assert_eq!(
            resp.unwrap(),
            true,
            "adding a new with an old source should set a completed flag"
        );
    }
}
