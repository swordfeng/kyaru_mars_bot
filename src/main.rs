use anyhow::{anyhow, Result};
use bk_tree::{BKTree, Metric};
use futures::StreamExt;
use image::GenericImageView;
use img_hash::{Hasher, HasherConfig, ImageHash};
use log::{debug, error, info};
use once_cell::sync::{Lazy, OnceCell};
use regex::Regex;
use serde_json::json;
use serde_json::value::Value;
use std::collections::HashMap;
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Seek, SeekFrom, Write};
use telegram_bot::*;

const MIN_IMAGE_HEIGHT: u32 = 480;
const MAX_PAGE_SIZE: usize = 65536;
const MAX_IMAGE_SIZE: usize = 33554432;
static TWITTER_CONTEXT: OnceCell<TwitterContext> = OnceCell::new();
static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| reqwest::Client::new());

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let token = env::var("TELEGRAM_BOT_TOKEN").expect("TELEGRAM_BOT_TOKEN not set");
    let api = Api::new(&token);
    let hasher = HasherConfig::new().preproc_dct().to_hasher();
    let mut img_db = ImageDatabase::new("images.db")?;

    match init_twitter_context().await {
        Ok(c) => TWITTER_CONTEXT
            .set(c)
            .map_err(|_| anyhow!("failed to set twitter context once_cell"))?,
        Err(e) => error!("{}", e),
    }

    let mut stream = api.stream();
    loop {
        if let Some(update) = stream.next().await {
            match update {
                Err(e) => error!("{}", e),
                Ok(update) => {
                    if let Err(e) = handle_update(update, &api, &token, &hasher, &mut img_db).await
                    {
                        error!("{}", e);
                    }
                }
            }
        }
    }
}

async fn handle_update(
    update: Update,
    api: &Api,
    token: &str,
    hasher: &Hasher,
    img_db: &mut ImageDatabase,
) -> Result<()> {
    if let UpdateKind::Message(message) = update.kind {
        debug!("Message: {:?}", &message);
        if let MessageKind::Photo {
            ref data,
            ref caption,
            ..
        } = message.kind
        {
            let largest_photo =
                data.iter().fold(
                    &data[0],
                    |ps1, ps2| if ps1.height < ps2.height { ps2 } else { ps1 },
                );
            debug!("Get photo: {:?}", largest_photo);
            let photo_file_response = api.send(largest_photo.get_file()).await?;
            let file_url = format!(
                "https://api.telegram.org/file/bot{}/{}",
                &token,
                photo_file_response
                    .file_path
                    .ok_or(anyhow!("Empty file url in Telegram response"))?
            );
            let file_content = CLIENT.get(&file_url).send().await?.bytes().await?;
            let img = image::load_from_memory(&file_content)?;
            let hash = hasher.hash_image(&img);
            debug!("Photo hash: {:?}", &hash);
            if let Some(cap) = caption {
                if cap == "!!hash" {
                    api.send(message.text_reply(format!("{:?}", hash.as_bytes())))
                        .await?;
                    return Ok(());
                }
            }
            if img.height() < MIN_IMAGE_HEIGHT {
                return Ok(());
            }
            if img_db.exists(message.chat.id(), &hash) {
                debug!("Hash exists");
                api.send(SeenItBefore::reply_to(message.chat.id(), message.id))
                    .await?;
            } else {
                img_db.add(message.chat.id(), hash)?;
                debug!("Hash added");
            }
        } else if let MessageKind::Text {
            ref data,
            ref entities,
            ..
        } = message.kind
        {
            let mut replied = false;
            for e in entities {
                let file_url = match e.kind {
                    MessageEntityKind::Url => {
                        extract_image_url(match data.get(e.offset as usize..e.length as usize) {
                            Some(url) => url,
                            None => continue,
                        })
                        .await
                    }
                    MessageEntityKind::TextLink(ref url) => extract_image_url(url).await,
                    _ => None,
                };
                if let Some(file_url) = file_url {
                    debug!("Get photo url: {:?}", &file_url);
                    let file_content =
                        read_max_bytes(&mut CLIENT.get(&file_url).send().await?, MAX_IMAGE_SIZE)
                            .await?;
                    let img = image::load_from_memory(&file_content)?;
                    if img.height() < MIN_IMAGE_HEIGHT {
                        continue;
                    }
                    let hash = hasher.hash_image(&img);
                    debug!("Photo hash: {:?}", &hash);
                    if img_db.exists(message.chat.id(), &hash) {
                        debug!("Hash exists");
                        if !replied {
                            api.send(SeenItBefore::reply_to(message.chat.id(), message.id))
                                .await?;
                            replied = true;
                        }
                    } else {
                        img_db.add(message.chat.id(), hash)?;
                        debug!("Hash added");
                    }
                }
            }
        }
    }
    Ok(())
}

async fn read_max_bytes(resp: &mut reqwest::Response, size_limit: usize) -> Result<Vec<u8>> {
    let mut total_len = 0;
    let mut data = vec![];
    data.reserve(size_limit);
    while let Some(chunk) = resp.chunk().await? {
        data.append(&mut chunk.to_vec());
        total_len += chunk.len();
        if total_len > size_limit {
            break;
        }
    }
    Ok(data)
}

async fn extract_image_url(url: &str) -> Option<String> {
    debug!("Extract: {}", url);
    static TWEET_URL: Lazy<Regex> =
        Lazy::new(|| Regex::new("//twitter.com/.*/status/([0-9]+)").unwrap());
    if let Some(c) = TWEET_URL.captures(url) {
        if let Some(photo_url) = extract_tweet_image(&c[1]).await {
            debug!("Extracted from twitter: {}", photo_url);
            return Some(photo_url);
        } else {
            return None;
        }
    }
    let mut resp = CLIENT.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let content_type = resp.headers()[reqwest::header::CONTENT_TYPE]
        .to_str()
        .ok()?;
    if content_type.starts_with("image/") {
        debug!("Extracted: {}", url);
        return Some(url.to_owned());
    }
    if !content_type.starts_with("text/html") {
        return None;
    }
    let data = read_max_bytes(&mut resp, MAX_PAGE_SIZE).await.ok()?;
    let data = std::str::from_utf8(&data).ok()?;
    static META_OG: Lazy<Regex> =
        Lazy::new(|| Regex::new("<meta[^>]* property=\"og:image\"[^>]*>").unwrap());
    static META_TW: Lazy<Regex> =
        Lazy::new(|| Regex::new("<meta[^>]* name=\"twitter:image(:src)?\"[^>]*>").unwrap());
    static CONTENT: Lazy<Regex> = Lazy::new(|| Regex::new("content=\"([^\"]*)\"").unwrap());
    if let Some(m) = META_OG.find(&data) {
        if let Some(c) = data
            .get(m.start()..m.end())
            .and_then(|line| CONTENT.captures(line))
        {
            debug!("Extracted: {}", &c[1]);
            return Some(c[1].to_owned());
        }
    }
    if let Some(m) = META_TW.find(&data) {
        if let Some(c) = data
            .get(m.start()..m.end())
            .and_then(|line| CONTENT.captures(line))
        {
            debug!("Extracted: {}", &c[1]);
            return Some(c[1].to_owned());
        }
    }
    None
}

async fn extract_tweet_image(id: &str) -> Option<String> {
    if let Some(TwitterContext { ref bearer, ref gt }) = TWITTER_CONTEXT.get() {
        let resp = CLIENT
            .get(&format!(
                "https://api.twitter.com/2/timeline/conversation/{}.json",
                id
            ))
            .query(&[
                ("include_entities", "true"),
                ("include_user_entities", "false"),
                ("count", "1"),
            ])
            .header("authorization", format!("Bearer {}", bearer))
            .header("x-guest-token", gt)
            .send()
            .await
            .ok()?
            .json::<Value>()
            .await
            .ok()?;
        let tweet = &resp["globalObjects"]["tweets"][id];
        if let Value::Array(ref media) = tweet["entities"]["media"] {
            for medium in media {
                if medium["type"] == json!("photo") {
                    return Some(medium["media_url_https"].as_str()?.to_owned());
                }
            }
        }
        if let Value::Array(ref media) = tweet["extended_entities"]["media"] {
            for medium in media {
                if medium["type"] == json!("photo") {
                    return Some(medium["media_url_https"].as_str()?.to_owned());
                }
            }
        }
    }
    None
}

struct Distance;

impl Metric<ImageHash> for Distance {
    fn distance(&self, a: &ImageHash, b: &ImageHash) -> u64 {
        a.dist(b) as u64
    }
}

struct ImageDatabase {
    m: HashMap<ChatId, BKTree<ImageHash, Distance>>,
    file: BufWriter<File>,
}

impl ImageDatabase {
    fn new(path: &str) -> Result<ImageDatabase> {
        let mut m = HashMap::new();
        let mut count = 0;
        let file = if let Ok(file) = File::open(path) {
            let mut reader = BufReader::new(file);
            let mut curpos = 0;
            loop {
                match rmp_serde::from_read::<_, (ChatId, String)>(&mut reader) {
                    Ok((cid, h)) => {
                        m.entry(cid)
                            .or_insert_with(|| BKTree::new(Distance))
                            .add(ImageHash::from_base64(&h)?);
                        debug!("Image DB insert {:?}", &h);
                        count += 1;
                    }
                    Err(err) => {
                        if let rmp_serde::decode::Error::InvalidMarkerRead(ref e) = err {
                            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                                break;
                            }
                        } else {
                            Err(err)?;
                        }
                    }
                }
                curpos = reader.seek(SeekFrom::Current(0))?;
            }
            let mut file = OpenOptions::new().write(true).open(path)?;
            file.set_len(curpos)?;
            file.seek(SeekFrom::End(0))?;
            file
        } else {
            File::create(path)?
        };
        info!("Image DB loaded, current size = {}", &count);
        Ok(ImageDatabase {
            m,
            file: BufWriter::new(file),
        })
    }

    fn exists(&self, cid: ChatId, h: &ImageHash) -> bool {
        if let Some(bkt) = self.m.get(&cid) {
            if let Some(_) = bkt.find(h, 5).next() {
                return true;
            }
        }
        false
    }

    fn add(&mut self, cid: ChatId, h: ImageHash) -> Result<()> {
        debug!("Image DB insert {:?}", &h);
        rmp_serde::encode::write(&mut self.file, &(cid, h.to_base64()))?;
        self.file.flush()?;
        self.m
            .entry(cid)
            .or_insert_with(|| BKTree::new(Distance))
            .add(h);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, PartialOrd)]
struct SeenItBefore {
    chat_id: ChatId,
    reply_to_message_id: MessageId,
}

impl SeenItBefore {
    fn reply_to(chat_id: ChatId, reply_to_message_id: MessageId) -> SeenItBefore {
        SeenItBefore {
            chat_id,
            reply_to_message_id,
        }
    }
}

impl ToMultipart for SeenItBefore {
    fn to_multipart(&self) -> Result<Multipart, telegram_bot_raw::requests::_base::Error> {
        Ok(vec![
            (
                "chat_id",
                telegram_bot_raw::requests::_base::MultipartValue::Text(
                    self.chat_id.to_string().into(),
                ),
            ),
            (
                "sticker",
                telegram_bot_raw::requests::_base::MultipartValue::Text(
                    "CAACAgUAAxkBAAMyX0Sjn0AB9RHDl1Y62MljVR2F_HkAAgYAAwfDqAvcvSc9SDpa3hsE".into(),
                ),
            ),
            (
                "reply_to_message_id",
                telegram_bot_raw::requests::_base::MultipartValue::Text(
                    self.reply_to_message_id.to_string().into(),
                ),
            ),
        ])
    }
}

impl Request for SeenItBefore {
    type Type = MultipartRequestType<Self>;
    type Response = JsonIdResponse<Message>;

    fn serialize(&self) -> Result<HttpRequest, telegram_bot_raw::requests::_base::Error> {
        Self::Type::serialize(RequestUrl::method("sendSticker"), self)
    }
}

struct TwitterContext {
    bearer: String,
    gt: String,
}

async fn init_twitter_context() -> Result<TwitterContext> {
    static TWITTER_GT: Lazy<Regex> = Lazy::new(|| Regex::new("gt=([0-9]+);").unwrap());
    static TWITTER_BEARER: Lazy<Regex> = Lazy::new(|| Regex::new("AAAAAAAA[^\"]+").unwrap());
    let twitter_page = CLIENT
        .get("https://twitter.com")
        .send()
        .await?
        .text()
        .await?;
    let gt = TWITTER_GT
        .captures(&twitter_page)
        .ok_or(anyhow!("guest_token not found on twitter"))?[1]
        .to_owned();
    static TWITTER_MAIN_SCRIPT: Lazy<Regex> = Lazy::new(|| {
        Regex::new("https://abs\\.twimg\\.com/responsive-web/client-web(-legacy)?/main\\.[a-zA-Z0-9_-]+\\.js").unwrap()
    });
    let main_script_src = TWITTER_MAIN_SCRIPT
        .captures(&twitter_page)
        .ok_or(anyhow!("main.js not found on twitter"))?[0]
        .to_owned();
    let main_script = CLIENT.get(&main_script_src).send().await?.text().await?;
    let bearer = TWITTER_BEARER
        .captures(&main_script)
        .ok_or(anyhow!("bearer not found on twitter"))?[0]
        .to_owned();
    debug!("Twitter bearer = {}, gt = {}", &bearer, &gt);
    Ok(TwitterContext { bearer, gt })
}
