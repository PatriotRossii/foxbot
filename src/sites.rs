use async_trait::async_trait;
use fautil::MatchType;
use reqwest::header;
use serde::Deserialize;
use std::collections::HashMap;
use tokio01::runtime::current_thread::block_on_all;

const USER_AGENT: &str = concat!(
    "t.me/FoxBot version ",
    env!("CARGO_PKG_VERSION"),
    " developed by @Syfaro"
);

#[derive(Clone, Debug, Default)]
pub struct PostInfo {
    /// File type, as a standard file extension (png, jpg, etc.)
    pub file_type: String,
    /// URL to full image
    pub url: String,
    /// If this result is personal
    pub personal: bool,
    /// URL to thumbnail, if available
    pub thumb: Option<String>,
    /// URL to original source of this image, if available
    pub source_link: Option<String>,
    /// Additional caption to add as a second result for the provided query
    pub extra_caption: Option<String>,
    /// Title for video results
    pub title: Option<String>,
}

fn get_file_ext(name: &str) -> Option<&str> {
    name.split('.')
        .last()
        .map(|ext| ext.split('?').next())
        .flatten()
}

#[derive(Fail, Debug)]
pub enum SiteError {
    #[fail(display = "http error")]
    Request(reqwest::Error),
    #[fail(display = "json parsing error")]
    JSON(serde_json::Error),
    #[fail(display = "missing required value")]
    Missing(std::option::NoneError),
    #[fail(display = "twitter error")]
    Twitter(egg_mode::error::Error),
    #[fail(display = "python error")]
    Python(String),
}

impl From<reqwest::Error> for SiteError {
    fn from(e: reqwest::Error) -> Self {
        SiteError::Request(e)
    }
}

impl From<serde_json::Error> for SiteError {
    fn from(e: serde_json::Error) -> Self {
        SiteError::JSON(e)
    }
}

impl From<egg_mode::error::Error> for SiteError {
    fn from(e: egg_mode::error::Error) -> Self {
        SiteError::Twitter(e)
    }
}

impl From<std::option::NoneError> for SiteError {
    fn from(e: std::option::NoneError) -> Self {
        SiteError::Missing(e)
    }
}

impl From<cfscrape::Error> for SiteError {
    fn from(e: cfscrape::Error) -> Self {
        SiteError::Python(match e {
            cfscrape::Error::Python(err) => format!("{:?}", err),
        })
    }
}

#[async_trait]
pub trait Site {
    fn name(&self) -> &'static str;
    async fn url_supported(&mut self, url: &str) -> bool;
    async fn get_images(
        &mut self,
        user_id: i32,
        url: &str,
    ) -> Result<Option<Vec<PostInfo>>, SiteError>;
}

pub struct Direct {
    client: reqwest::Client,
    fautil: std::sync::Arc<fautil::FAUtil>,
}

impl Direct {
    const EXTENSIONS: &'static [&'static str] = &["png", "jpg", "jpeg", "gif"];
    const TYPES: &'static [&'static str] = &["image/png", "image/jpeg", "image/gif"];

    pub fn new(fautil: std::sync::Arc<fautil::FAUtil>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("Unable to create client");

        Self { fautil, client }
    }

    async fn reverse_search(&self, url: &str) -> Option<fautil::File> {
        let image = self.client.get(url).send().await;

        let image = match image {
            Ok(res) => res.bytes().await,
            Err(_) => return None,
        };

        let body = match image {
            Ok(body) => body,
            Err(_) => return None,
        };

        let results = self.fautil.image_search(&body, MatchType::Exact).await;

        match results {
            Ok(results) => results.matches.into_iter().next(),
            Err(_) => None,
        }
    }
}

#[async_trait]
impl Site for Direct {
    fn name(&self) -> &'static str {
        "direct links"
    }

    async fn url_supported(&mut self, url: &str) -> bool {
        // If the URL extension isn't one in our list, ignore.
        if !Direct::EXTENSIONS.iter().any(|ext| url.ends_with(ext)) {
            return false;
        }

        // Make a HTTP HEAD request to determine the Content-Type.
        let resp = match self
            .client
            .head(url)
            .header(header::USER_AGENT, USER_AGENT)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(_) => return false,
        };

        if !resp.status().is_success() {
            return false;
        }

        let content_type = match resp.headers().get(reqwest::header::CONTENT_TYPE) {
            Some(content_type) => content_type,
            None => return false,
        };

        // Return if the Content-Type is in our list.
        Direct::TYPES.iter().any(|t| content_type == t)
    }

    async fn get_images(
        &mut self,
        _user_id: i32,
        url: &str,
    ) -> Result<Option<Vec<PostInfo>>, SiteError> {
        let u = url.to_string();
        let mut source_link = None;

        if let Ok(result) =
            tokio::time::timeout(std::time::Duration::from_secs(4), self.reverse_search(&u)).await
        {
            tracing::trace!("got result from reverse search");
            if let Some(post) = result {
                tracing::debug!("found ID of post matching: {}", post.id);
                source_link = Some(post.url());
            } else {
                tracing::trace!("no posts matched");
            }
        } else {
            tracing::debug!("reverse search timed out");
        }

        Ok(Some(vec![PostInfo {
            file_type: get_file_ext(url).unwrap().to_string(),
            url: u.clone(),
            source_link,
            ..Default::default()
        }]))
    }
}

pub struct E621 {
    show: regex::Regex,
    data: regex::Regex,

    client: reqwest::Client,
}

#[derive(Deserialize)]
struct E621Post {
    id: i32,
    file_url: String,
    preview_url: String,
    file_ext: String,
}

impl E621 {
    pub fn new() -> Self {
        Self {
            show: regex::Regex::new(r"https?://(?P<host>e(?:621|926)\.net)/post/show/(?P<id>\d+)(?:/(?P<tags>.+))?").unwrap(),
            data: regex::Regex::new(r"https?://(?P<host>static\d+\.e(?:621|926)\.net)/data/(?:(?P<modifier>sample|preview)/)?[0-9a-f]{2}/[0-9a-f]{2}/(?P<md5>[0-9a-f]{32})\.(?P<ext>.+)").unwrap(),

            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Site for E621 {
    fn name(&self) -> &'static str {
        "e621"
    }

    async fn url_supported(&mut self, url: &str) -> bool {
        self.show.is_match(url) || self.data.is_match(url)
    }

    async fn get_images(
        &mut self,
        _user_id: i32,
        url: &str,
    ) -> Result<Option<Vec<PostInfo>>, SiteError> {
        let endpoint = if self.show.is_match(url) {
            let captures = self.show.captures(url).unwrap();
            let id = &captures["id"];

            format!("https://e621.net/post/show.json?id={}", id)
        } else {
            let captures = self.data.captures(url).unwrap();
            let md5 = &captures["md5"];

            format!("https://e621.net/post/show.json?md5={}", md5)
        };

        let resp: E621Post = self
            .client
            .get(&endpoint)
            .header(header::USER_AGENT, USER_AGENT)
            .send()
            .await?
            .json()
            .await?;

        Ok(Some(vec![PostInfo {
            file_type: resp.file_ext,
            url: resp.file_url,
            thumb: Some(resp.preview_url),
            source_link: Some(format!("https://e621.net/post/show/{}", resp.id)),
            ..Default::default()
        }]))
    }
}

pub struct Twitter {
    matcher: regex::Regex,
    consumer: egg_mode::KeyPair,
    token: egg_mode::Token,
    conn: quaint::pooled::Quaint,
}

impl Twitter {
    pub fn new(
        consumer_key: String,
        consumer_secret: String,
        conn: quaint::pooled::Quaint,
    ) -> Self {
        use egg_mode::KeyPair;

        let consumer = KeyPair::new(consumer_key, consumer_secret);
        let token = block_on_all(egg_mode::bearer_token(&consumer)).unwrap();

        Self {
            matcher: regex::Regex::new(
                r"https://(?:mobile\.)?twitter.com/(?:\w+)/status/(?P<id>\d+)",
            )
            .unwrap(),
            consumer,
            token,
            conn,
        }
    }
}

#[async_trait]
impl Site for Twitter {
    fn name(&self) -> &'static str {
        "Twitter"
    }

    async fn url_supported(&mut self, url: &str) -> bool {
        self.matcher.is_match(url)
    }

    async fn get_images(
        &mut self,
        user_id: i32,
        url: &str,
    ) -> Result<Option<Vec<PostInfo>>, SiteError> {
        use quaint::prelude::*;

        let captures = self.matcher.captures(url).unwrap();
        let id = captures["id"].to_owned().parse::<u64>().unwrap();

        tracing::trace!(user_id, "attempting to find saved credentials",);

        let conn = self.conn.check_out().await.expect("Unable to get db conn");
        let result = conn
            .select(
                Select::from_table("twitter_account")
                    .column("consumer_key")
                    .column("consumer_secret")
                    .so_that("user_id".equals(user_id)),
            )
            .await
            .expect("Unable to query db");

        let token = match result.first() {
            Some(result) => egg_mode::Token::Access {
                consumer: self.consumer.clone(),
                access: egg_mode::KeyPair::new(
                    result["consumer_key"].to_string().unwrap(),
                    result["consumer_secret"].to_string().unwrap(),
                ),
            },
            _ => self.token.clone(),
        };

        let tweet = match block_on_all(egg_mode::tweet::show(id, &token)) {
            Ok(tweet) => tweet.response,
            Err(e) => return Err(e.into()),
        };

        let user = tweet.user.unwrap();

        let media = match tweet.extended_entities {
            Some(entity) => entity.media,
            None => return Ok(None),
        };

        let text = tweet.text.clone();

        Ok(Some(
            media
                .into_iter()
                .map(|item| match get_best_video(&item) {
                    Some(video_url) => PostInfo {
                        file_type: get_file_ext(video_url).unwrap().to_owned(),
                        url: video_url.to_string(),
                        thumb: Some(format!("{}:thumb", item.media_url_https.clone())),
                        source_link: Some(item.expanded_url),
                        personal: user.protected,
                        title: Some(user.screen_name.clone()),
                        extra_caption: Some(text.clone()),
                    },
                    None => PostInfo {
                        file_type: get_file_ext(&item.media_url_https).unwrap().to_owned(),
                        url: item.media_url_https.clone(),
                        thumb: Some(format!("{}:thumb", item.media_url_https.clone())),
                        source_link: Some(item.expanded_url),
                        personal: user.protected,
                        ..Default::default()
                    },
                })
                .collect(),
        ))
    }
}

fn get_best_video(media: &egg_mode::entities::MediaEntity) -> Option<&str> {
    let video_info = match &media.video_info {
        Some(video_info) => video_info,
        None => return None,
    };

    let highest_bitrate = video_info
        .variants
        .iter()
        .max_by_key(|video| video.bitrate.unwrap_or(0))
        .unwrap();

    Some(&highest_bitrate.url)
}

pub struct FurAffinity {
    cookies: std::collections::HashMap<String, String>,
    fapi: fautil::FAUtil,
    submission: scraper::Selector,
    client: reqwest::Client,
}

impl FurAffinity {
    pub fn new(cookies: (String, String), util_api: String) -> Self {
        let mut c = std::collections::HashMap::new();

        c.insert("a".into(), cookies.0);
        c.insert("b".into(), cookies.1);

        Self {
            cookies: c,
            fapi: fautil::FAUtil::new(util_api),
            submission: scraper::Selector::parse("#submissionImg").unwrap(),
            client: reqwest::Client::new(),
        }
    }

    async fn load_direct_url(&self, url: &str) -> Result<Option<PostInfo>, SiteError> {
        let url = if url.starts_with("http://") {
            url.replace("http://", "https://")
        } else {
            url.to_string()
        };

        let sub: fautil::File = match self.fapi.lookup_url(&url).await {
            Ok(mut results) if !results.is_empty() => results.remove(0),
            _ => {
                return Ok(Some(PostInfo {
                    file_type: get_file_ext(&url).unwrap().to_string(),
                    url: url.clone(),
                    ..Default::default()
                }));
            }
        };

        Ok(Some(PostInfo {
            file_type: get_file_ext(&sub.filename).unwrap().to_string(),
            url: sub.url.clone(),
            source_link: Some(sub.url()),
            ..Default::default()
        }))
    }

    fn stringify_cookies(&self) -> String {
        let mut cookies = vec![];
        for (name, value) in &self.cookies {
            cookies.push(format!("{}={}", name, value));
        }
        cookies.join("; ")
    }

    async fn load_submission(&mut self, url: &str) -> Result<Option<PostInfo>, SiteError> {
        let resp = self
            .client
            .get(url)
            .header(header::COOKIE, self.stringify_cookies())
            .header(header::USER_AGENT, USER_AGENT)
            .send()
            .await?;

        let resp = if resp.status() == 429 || resp.status() == 503 {
            let cfscrape::CfscrapeData { cookies, .. } =
                cfscrape::get_cookie_string(url, Some(USER_AGENT))?;
            let cookies = cookies.split("; ");
            for cookie in cookies {
                let mut parts = cookie.split('=');
                let name = parts.next().expect("Missing cookie name");
                let value = parts.next().expect("Missing cookie value");

                self.cookies.insert(name.into(), value.into());
            }

            self.client
                .get(url)
                .header(header::COOKIE, self.stringify_cookies())
                .header(header::USER_AGENT, USER_AGENT)
                .send()
                .await?
                .text()
                .await?
        } else {
            resp.text().await?
        };

        let body = scraper::Html::parse_document(&resp);
        let img = match body.select(&self.submission).next() {
            Some(img) => img,
            None => return Ok(None),
        };

        let image_url = format!("https:{}", img.value().attr("src")?);

        Ok(Some(PostInfo {
            file_type: get_file_ext(&image_url).unwrap().to_string(),
            url: image_url.clone(),
            source_link: Some(url.to_string()),
            ..Default::default()
        }))
    }
}

#[async_trait]
impl Site for FurAffinity {
    fn name(&self) -> &'static str {
        "FurAffinity"
    }

    async fn url_supported(&mut self, url: &str) -> bool {
        url.contains("furaffinity.net/view/")
            || url.contains("furaffinity.net/full/")
            || url.contains("facdn.net/art/")
    }

    async fn get_images(
        &mut self,
        _user_id: i32,
        url: &str,
    ) -> Result<Option<Vec<PostInfo>>, SiteError> {
        let image = if url.contains("facdn.net/art/") {
            self.load_direct_url(url).await
        } else {
            self.load_submission(url).await
        };

        image.map(|sub| sub.map(|post| vec![post]))
    }
}

pub struct Mastodon {
    instance_cache: HashMap<String, bool>,
    matcher: regex::Regex,
}

#[derive(Deserialize)]
struct MastodonStatus {
    url: String,
    media_attachments: Vec<MastodonMediaAttachments>,
}

#[derive(Deserialize)]
struct MastodonMediaAttachments {
    url: String,
    preview_url: String,
}

impl Mastodon {
    pub fn new() -> Self {
        Self {
            instance_cache: HashMap::new(),
            matcher: regex::Regex::new(
                r#"(?P<host>https?://(?:\S+))/(?:notice|users/\w+/statuses|@\w+)/(?P<id>\d+)"#,
            )
            .unwrap(),
        }
    }
}

#[async_trait]
impl Site for Mastodon {
    fn name(&self) -> &'static str {
        "Mastodon"
    }

    async fn url_supported(&mut self, url: &str) -> bool {
        let captures = match self.matcher.captures(url) {
            Some(captures) => captures,
            None => return false,
        };

        let base = captures["host"].to_owned();

        if let Some(is_masto) = self.instance_cache.get(&base) {
            if !is_masto {
                return false;
            }
        }

        let resp = match reqwest::Client::new()
            .head(&format!("{}/api/v1/instance", base))
            .header(header::USER_AGENT, USER_AGENT)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(_) => {
                self.instance_cache.insert(base, false);
                return false;
            }
        };

        if !resp.status().is_success() {
            self.instance_cache.insert(base, false);
            return false;
        }

        true
    }

    async fn get_images(
        &mut self,
        _user_id: i32,
        url: &str,
    ) -> Result<Option<Vec<PostInfo>>, SiteError> {
        let captures = self.matcher.captures(url).unwrap();

        let base = captures["host"].to_owned();
        let status_id = captures["id"].to_owned();

        let json: MastodonStatus = reqwest::Client::new()
            .get(&format!("{}/api/v1/statuses/{}", base, status_id))
            .header(header::USER_AGENT, USER_AGENT)
            .send()
            .await?
            .json()
            .await?;

        if json.media_attachments.is_empty() {
            return Ok(None);
        }

        Ok(Some(
            json.media_attachments
                .iter()
                .map(|media| PostInfo {
                    file_type: get_file_ext(&media.url).unwrap().to_owned(),
                    url: media.url.clone(),
                    thumb: Some(media.preview_url.clone()),
                    source_link: Some(json.url.clone()),
                    ..Default::default()
                })
                .collect(),
        ))
    }
}

pub struct Weasyl {
    api_key: String,
    matcher: regex::Regex,
}

impl Weasyl {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            matcher: regex::Regex::new(r#"https?://www\.weasyl\.com/(?:(?:~|%7)(?:\w+)/submissions|submission)/(?P<id>\d+)(?:/\S+)"#).unwrap(),
        }
    }
}

#[async_trait]
impl Site for Weasyl {
    fn name(&self) -> &'static str {
        "Weasyl"
    }

    async fn url_supported(&mut self, url: &str) -> bool {
        self.matcher.is_match(url)
    }

    async fn get_images(
        &mut self,
        _user_id: i32,
        url: &str,
    ) -> Result<Option<Vec<PostInfo>>, SiteError> {
        let captures = self.matcher.captures(url).unwrap();
        let sub_id = captures["id"].to_owned();

        let resp: serde_json::Value = reqwest::Client::new()
            .get(&format!(
                "https://www.weasyl.com/api/submissions/{}/view",
                sub_id
            ))
            .header("X-Weasyl-API-Key", self.api_key.as_bytes())
            .header(header::USER_AGENT, USER_AGENT)
            .send()
            .await?
            .json()
            .await?;

        let submissions = resp
            .as_object()?
            .get("media")?
            .as_object()?
            .get("submission")?
            .as_array()?;

        if submissions.is_empty() {
            return Ok(None);
        }

        let thumbs = resp
            .as_object()?
            .get("media")?
            .as_object()?
            .get("thumbnail")?
            .as_array()?;

        Ok(Some(
            submissions
                .iter()
                .zip(thumbs)
                .map(|(sub, thumb)| {
                    let sub_url = sub.get("url").unwrap().as_str().unwrap().to_owned();
                    let thumb_url = thumb.get("url").unwrap().as_str().unwrap().to_owned();

                    PostInfo {
                        file_type: get_file_ext(&sub_url).unwrap().to_owned(),
                        url: sub_url.clone(),
                        thumb: Some(thumb_url),
                        source_link: Some(url.to_string()),
                        ..Default::default()
                    }
                })
                .collect(),
        ))
    }
}
