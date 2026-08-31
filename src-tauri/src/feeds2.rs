use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, COOKIE, REFERER, USER_AGENT};
use scraper::{Html, Selector};
use serde_json::{json, Value};

use crate::qlogin::QLoginState;

const FEEDS2_URL: &str =
    "https://h5.qzone.qq.com/proxy/domain/ic2.qzone.qq.com/cgi-bin/feeds/feeds2_html_pav_all";
const FEEDS2_LEGACY_URL: &str =
    "https://user.qzone.qq.com/proxy/domain/ic2.qzone.qq.com/cgi-bin/feeds/feeds2_html_pav_all";
const FEEDS2_PAGE_SIZE: i64 = 30;
const FEEDS2_DESKTOP_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36";
const SUPPLEMENT_CHECKPOINT_PREFIX: &str = "feeds2:offset=";

pub const EARLIEST_ARCHIVE_YEAR: i32 = 2008;
pub const DEFAULT_ARCHIVE_TARGET_YEAR: i32 = 2017;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveDepthOption {
    pub target_year: i32,
    pub max_offset: i64,
    pub label: String,
}

/// 根据目标年份估算 feeds2 补充源需要扫描的最大 offset。
/// 算法参考 qzone-history 的实测推荐值。
pub fn recommend_max_offset(target_year: i32) -> i64 {
    if target_year <= 0 {
        return 0;
    }
    match target_year {
        year if year >= 2024 => 1_500,
        2023 => 2_500,
        2022 => 3_500,
        2021 => 5_000,
        2020 => 8_000,
        2019 => 12_000,
        2018 => 18_000,
        2017 => 25_000,
        2016 => 35_000,
        2015 => 50_000,
        2014 => 80_000,
        2013 => 90_000,
        2012 => 100_000,
        2011 => 110_000,
        2010 => 120_000,
        2009 => 130_000,
        _ => 150_000,
    }
}

pub fn list_depth_options(current_year: i32) -> Vec<ArchiveDepthOption> {
    let mut options = vec![ArchiveDepthOption {
        target_year: 0,
        max_offset: 0,
        label: "仅主源（mobile get_feeds，不启用 feeds2 深扫）".into(),
    }];
    for year in (EARLIEST_ARCHIVE_YEAR..=current_year).rev() {
        let max_offset = recommend_max_offset(year);
        options.push(ArchiveDepthOption {
            target_year: year,
            max_offset,
            label: format!("{year} 年及更早（offset ≤ {max_offset}）"),
        });
    }
    options
}

pub fn current_archive_year() -> i32 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    chrono_offset_year(now)
}

fn chrono_offset_year(timestamp: i64) -> i32 {
    let mut days = timestamp.div_euclid(86_400);
    let mut year = 1970_i32;
    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if days < days_in_year {
            return year;
        }
        days -= days_in_year;
        year += 1;
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivityKind {
    Moment,
    Forward,
    Like,
    Comment,
    BoardMessage,
    BoardReply,
    View,
    Other,
}

#[derive(Debug, Clone)]
struct ParsedActivity {
    sender_uin: String,
    sender_name: String,
    content: String,
    time_text: String,
    event_time: i64,
    image_urls: Vec<String>,
    kind: ActivityKind,
}

pub(crate) struct SupplementFeedPage {
    pub feeds: Vec<Value>,
    pub next_offset: i64,
    pub has_more: bool,
}

pub(crate) fn parse_supplement_checkpoint(cursor: &str) -> Option<i64> {
    cursor
        .strip_prefix(SUPPLEMENT_CHECKPOINT_PREFIX)
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|offset| *offset >= 0)
}

pub(crate) fn format_supplement_checkpoint(offset: i64) -> String {
    format!("{SUPPLEMENT_CHECKPOINT_PREFIX}{offset}")
}

pub(crate) fn is_supplement_checkpoint(cursor: &str) -> bool {
    cursor.starts_with(SUPPLEMENT_CHECKPOINT_PREFIX)
}

pub(crate) async fn fetch_supplement_page(
    login: &QLoginState,
    offset: i64,
    max_offset: i64,
) -> Result<SupplementFeedPage, String> {
    if max_offset <= 0 || offset > max_offset {
        return Ok(SupplementFeedPage {
            feeds: Vec::new(),
            next_offset: offset,
            has_more: false,
        });
    }
    let auth = login.qzone_auth().await?;
    let (raw, processed_html) =
        fetch_feeds2_body(login, &auth.uin, &auth.cookie_header, auth.g_tk, offset).await?;
    let activities = parse_activities_from_html(&processed_html, &auth.uin)?;
    let feeds = activities
        .into_iter()
        .map(|activity| activity_to_feed_value(&activity, &auth.uin))
        .collect::<Vec<_>>();
    let advanced = if feeds.is_empty() {
        offset.saturating_add(FEEDS2_PAGE_SIZE)
    } else {
        offset.saturating_add(feeds.len() as i64)
    };
    let has_more = has_more_feeds(&raw) && !feeds.is_empty() && advanced <= max_offset;
    Ok(SupplementFeedPage {
        feeds,
        next_offset: advanced,
        has_more,
    })
}

async fn fetch_feeds2_body(
    login: &QLoginState,
    uin: &str,
    cookie_header: &str,
    g_tk: i64,
    offset: i64,
) -> Result<(String, String), String> {
    let query = [
        ("uin", uin.to_string()),
        ("begin_time", "0".into()),
        ("end_time", "0".into()),
        ("getappnotification", "1".into()),
        ("getnotifi", "1".into()),
        ("has_get_key", "0".into()),
        ("offset", offset.to_string()),
        ("set", "0".into()),
        ("count", FEEDS2_PAGE_SIZE.to_string()),
        ("useutf8", "1".into()),
        ("outputhtmlfeed", "1".into()),
        ("scope", "1".into()),
        ("format", "jsonp".into()),
        ("g_tk", g_tk.to_string()),
    ];
    let referer = format!("https://user.qzone.qq.com/{uin}/main");
    let client = login.client();
    let mut last_error = "feeds2 请求失败".to_string();
    for attempt in 1..=3u32 {
        for url in [FEEDS2_URL, FEEDS2_LEGACY_URL] {
            match client
                .get(url)
                .query(&query)
                .header(ACCEPT, "*/*")
                .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.8")
                .header(COOKIE, cookie_header)
                .header(REFERER, &referer)
                .header(USER_AGENT, FEEDS2_DESKTOP_UA)
                .header("Sec-Fetch-Dest", "empty")
                .header("Sec-Fetch-Mode", "cors")
                .header("Sec-Fetch-Site", "same-origin")
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    let body = response
                        .text()
                        .await
                        .unwrap_or_else(|error| format!("读取 feeds2 响应失败：{error}"));
                    if status.is_success() {
                        if body.contains("waf.tencent.com") {
                            last_error = "feeds2 请求被腾讯 WAF 拦截".into();
                            continue;
                        }
                        if body.contains("need login") {
                            return Err("feeds2 补充源需要重新登录 QQ 空间".into());
                        }
                        let processed = process_feed_response(&body);
                        return Ok((body, processed));
                    }
                    last_error = format!("feeds2 补充源 HTTP {status}");
                }
                Err(error) => last_error = format!("feeds2 请求失败：{error}"),
            }
        }
        if attempt < 3 {
            tokio::time::sleep(std::time::Duration::from_secs(attempt as u64 * 2)).await;
        }
    }
    Err(last_error)
}

fn process_feed_response(message: &str) -> String {
    if message.contains("waf.tencent.com") {
        return String::new();
    }
    let message = unescape_feed_text(message);
    if message.contains("_Callback(") || message.contains("data:[") {
        if let Some(extracted) = extract_h5_feeds_html(&message) {
            return collapse_whitespace(&extracted);
        }
    }
    process_old_html(&message)
}

fn unescape_feed_text(message: &str) -> String {
    let re = Regex::new(r"\\x[0-9a-fA-F]{2}").expect("hex escape regex");
    let decoded = re
        .replace_all(message, |caps: &regex::Captures| {
            u8::from_str_radix(&caps[0][2..], 16)
                .map(|byte| char::from(byte).to_string())
                .unwrap_or_else(|_| caps[0].to_string())
        })
        .into_owned();
    decoded
        .replace("\\/", "/")
        .replace("\\'", "'")
        .replace("\\\"", "\"")
}

fn extract_h5_feeds_html(message: &str) -> Option<String> {
    let mut parts = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = message[search_from..].find("html:'") {
        let start = search_from + rel + "html:'".len();
        let rest = &message[start..];
        let mut end = None;
        for marker in ["',is_public_pav", "',opuin"] {
            if let Some(pos) = rest.find(marker) {
                end = Some(end.map_or(pos, |current: usize| current.min(pos)));
            }
        }
        let end = end?;
        parts.push(rest[..end].to_string());
        search_from = start + end + 1;
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(""))
    }
}

fn process_old_html(message: &str) -> String {
    let mut text = unescape_feed_text(message);
    for (start, end) in [
        ("html:'", "',opuin"),
        ("html:\"", "\",opuin"),
        ("\"html\":\"", "\",\"opuin"),
    ] {
        if let Some(extracted) = extract_between(&text, start, end) {
            text = extracted;
            break;
        }
    }
    collapse_whitespace(&text.replace('\\', ""))
}

fn extract_between(source: &str, start: &str, end: &str) -> Option<String> {
    let start_index = source.find(start)? + start.len();
    let end_index = source[start_index..].find(end)? + start_index;
    Some(source[start_index..end_index].to_string())
}

fn collapse_whitespace(input: &str) -> String {
    Regex::new(r"\s+")
        .expect("whitespace regex")
        .replace_all(input.trim(), " ")
        .into_owned()
}

fn has_more_feeds(message: &str) -> bool {
    message.contains("hasMoreFeeds:true")
}

fn parse_activities_from_html(
    processed_html: &str,
    owner_uin: &str,
) -> Result<Vec<ParsedActivity>, String> {
    if processed_html.trim().is_empty() || !processed_html.contains("li") {
        return Ok(Vec::new());
    }
    let document = Html::parse_document(processed_html);
    let item_selector = Selector::parse("li.f-single.f-s-s").map_err(|error| error.to_string())?;
    let name_selector =
        Selector::parse("a.f-name.q_namecard").map_err(|error| error.to_string())?;
    let time_selector = Selector::parse("div.info-detail").map_err(|error| error.to_string())?;
    let content_selector =
        Selector::parse("p.txt-box-title.ellipsis-one").map_err(|error| error.to_string())?;
    let image_selector = Selector::parse("a.img-item img").map_err(|error| error.to_string())?;
    let state_selector = Selector::parse("span.state").map_err(|error| error.to_string())?;
    let reprint_selector =
        Selector::parse("div.f-reprint div.f-info").map_err(|error| error.to_string())?;

    let mut activities = Vec::new();
    for item in document.select(&item_selector) {
        let sender_name = item
            .select(&name_selector)
            .next()
            .map(|node| collapse_whitespace(&node.text().collect::<String>()))
            .unwrap_or_default();
        let sender_uin = item
            .select(&name_selector)
            .next()
            .and_then(|node| node.value().attr("link"))
            .map(|link| link.trim_start_matches("nameCard_").to_string())
            .unwrap_or_default();
        let time_text = item
            .select(&time_selector)
            .next()
            .map(|node| collapse_whitespace(&node.text().collect::<String>()))
            .unwrap_or_default();
        let mut content = item
            .select(&content_selector)
            .next()
            .map(|node| collapse_whitespace(&node.text().collect::<String>()))
            .unwrap_or_default();
        content = content.replace('\u{00a0}', " ");
        let image_urls = item
            .select(&image_selector)
            .filter_map(|node| node.value().attr("src").map(str::to_owned))
            .collect::<Vec<_>>();
        let state_text = item
            .select(&state_selector)
            .map(|node| collapse_whitespace(&node.text().collect::<String>()))
            .collect::<Vec<_>>()
            .join(" ");
        let has_reprint = item.select(&reprint_selector).next().is_some();
        if has_reprint {
            if let Some(forward) = item.select(&reprint_selector).next() {
                content = collapse_whitespace(&forward.text().collect::<String>());
            }
        }
        let kind = classify_activity(&state_text, &sender_uin, owner_uin, has_reprint);
        let event_time = parse_cn_time(&time_text).unwrap_or(0);
        activities.push(ParsedActivity {
            sender_uin,
            sender_name,
            content,
            time_text,
            event_time,
            image_urls,
            kind,
        });
    }
    Ok(activities)
}

fn classify_activity(
    state_text: &str,
    sender_uin: &str,
    owner_uin: &str,
    has_reprint: bool,
) -> ActivityKind {
    if state_text.contains("留言") && state_text.contains("回复") {
        ActivityKind::BoardReply
    } else if state_text.contains("留言") {
        ActivityKind::BoardMessage
    } else if state_text.contains("赞了我的说说") {
        ActivityKind::Like
    } else if state_text.contains("查看了我的说说") || state_text.contains("访问了我的主页")
    {
        ActivityKind::View
    } else if state_text.contains("评论") || state_text.contains("回复") {
        ActivityKind::Comment
    } else if state_text.contains("发表了说说") || state_text.contains("发表说说") {
        ActivityKind::Moment
    } else if state_text.contains("说说") && sender_uin == owner_uin {
        ActivityKind::Moment
    } else if has_reprint {
        ActivityKind::Forward
    } else {
        ActivityKind::Other
    }
}

fn parse_cn_time(time_str: &str) -> Option<i64> {
    let time_str = time_str.trim();
    if time_str.is_empty() {
        return None;
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let local = chrono_now_parts(now);
    let layouts = [
        ("%Y年%m月%d日 %H:%M:%S", 0),
        ("%Y年%m月%d日 %H:%M", 0),
        ("%Y-%m-%d %H:%M:%S", 0),
        ("%Y-%m-%d %H:%M", 0),
        ("%m月%d日 %H:%M", 1),
        ("昨天 %H:%M", 2),
        ("%H:%M", 3),
    ];
    for (layout, kind) in layouts {
        if let Some(parsed) = parse_with_layout(time_str, layout, local, kind) {
            return Some(parsed);
        }
    }
    None
}

#[derive(Clone, Copy)]
struct LocalParts {
    year: i32,
    month: u32,
    day: u32,
}

fn chrono_now_parts(now: i64) -> LocalParts {
    // Avoid adding chrono: derive calendar parts from local offset approximation.
    let days = now / 86_400;
    let mut year = 1970i32;
    let mut remaining = days;
    loop {
        let days_in_year = if is_leap(year) { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        year += 1;
    }
    let month_lengths = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1u32;
    for length in month_lengths {
        if remaining < length {
            return LocalParts {
                year,
                month,
                day: (remaining + 1) as u32,
            };
        }
        remaining -= length;
        month += 1;
    }
    LocalParts {
        year,
        month: 12,
        day: 31,
    }
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

fn parse_with_layout(input: &str, layout: &str, today: LocalParts, kind: u8) -> Option<i64> {
    let numbers = Regex::new(r"\d+")
        .ok()?
        .find_iter(input)
        .map(|m| m.as_str().parse::<i32>().ok())
        .collect::<Option<Vec<_>>>()?;
    match (layout, kind) {
        (_, 3) if numbers.len() >= 2 => to_unix(
            today.year,
            today.month as i32,
            today.day as i32,
            numbers[0],
            numbers[1],
            0,
        ),
        (_, 2) if numbers.len() >= 2 => {
            let yesterday =
                to_unix(today.year, today.month as i32, today.day as i32, 0, 0, 0)? - 86_400;
            let parts = unix_to_parts(yesterday);
            to_unix(
                parts.year,
                parts.month as i32,
                parts.day as i32,
                numbers[0],
                numbers[1],
                0,
            )
        }
        (_, 1) if numbers.len() >= 4 => to_unix(
            today.year, numbers[0], numbers[1], numbers[2], numbers[3], 0,
        ),
        (_, 0) if numbers.len() >= 5 => to_unix(
            numbers[0],
            numbers[1],
            numbers[2],
            numbers[3],
            numbers[4],
            numbers.get(5).copied().unwrap_or(0),
        ),
        _ => None,
    }
}

fn to_unix(year: i32, month: i32, day: i32, hour: i32, minute: i32, second: i32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut days = 0i64;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    let month_lengths = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    for index in 0..(month as usize - 1) {
        days += month_lengths[index] as i64;
    }
    days += (day - 1) as i64;
    Some(days * 86_400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64)
}

fn unix_to_parts(unix: i64) -> LocalParts {
    chrono_now_parts(unix)
}

fn synthetic_cell_id(
    owner_uin: &str,
    content: &str,
    author_uin: &str,
    published_at: i64,
) -> String {
    let seed = format!("{owner_uin}|{content}|{author_uin}|{published_at}");
    let hash = seed.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    format!("feeds2:{hash:016x}")
}

fn activity_to_feed_value(activity: &ParsedActivity, owner_uin: &str) -> Value {
    let (event_type, title, event_summary, original_author_uin, original_author_name, appid) =
        match activity.kind {
            ActivityKind::Like => (
                217,
                Some("赞了我".to_string()),
                None,
                activity.sender_uin.clone(),
                activity.sender_name.clone(),
                311,
            ),
            ActivityKind::Comment => (
                2,
                None,
                Some(activity.content.clone()),
                owner_uin.to_string(),
                String::new(),
                311,
            ),
            ActivityKind::BoardMessage | ActivityKind::BoardReply => (
                334,
                Some("留言".to_string()),
                Some(activity.content.clone()),
                activity.sender_uin.clone(),
                activity.sender_name.clone(),
                334,
            ),
            ActivityKind::View => (
                218,
                Some("查看了我的说说".to_string()),
                None,
                owner_uin.to_string(),
                String::new(),
                311,
            ),
            ActivityKind::Forward => (
                312,
                Some("转发了".to_string()),
                None,
                activity.sender_uin.clone(),
                activity.sender_name.clone(),
                311,
            ),
            ActivityKind::Moment => (
                202,
                None,
                None,
                activity.sender_uin.clone(),
                activity.sender_name.clone(),
                311,
            ),
            ActivityKind::Other => (0, None, None, owner_uin.to_string(), String::new(), 311),
        };

    let published_at = activity.event_time;
    let cell_id = synthetic_cell_id(
        owner_uin,
        &activity.content,
        &original_author_uin,
        published_at,
    );
    let feed_key = format!(
        "feeds2:{event_type}:{cell_id}:{}:{}",
        published_at, activity.sender_uin
    );
    let pictures = if activity.image_urls.is_empty() {
        Value::Null
    } else {
        json!({
            "picdata": {
                "pic": activity.image_urls.iter().map(|url| json!({
                    "photourl": [{ "url": url }]
                })).collect::<Vec<_>>()
            }
        })
    };
    let comments = if activity.kind == ActivityKind::Comment {
        json!({
            "main_comment": {
                "content": activity.content,
                "date": activity.event_time,
                "user": {
                    "uin": activity.sender_uin,
                    "nickname": activity.sender_name
                }
            }
        })
    } else {
        Value::Null
    };

    json!({
        "comm": {
            "feedskey": feed_key,
            "subid": event_type,
            "time": activity.event_time
        },
        "original": {
            "cell_id": { "cellid": cell_id },
            "cell_summary": { "summary": activity.content },
            "cell_userinfo": {
                "user": {
                    "uin": original_author_uin,
                    "nickname": original_author_name
                }
            },
            "cell_comm": {
                "time": published_at,
                "appid": appid,
                "feedskey": feed_key
            },
            "cell_pic": pictures,
            "cell_comment": comments
        },
        "title": title.as_ref().map(|value| json!({ "title": value })).unwrap_or(Value::Null),
        "summary": event_summary.as_ref().map(|value| json!({ "summary": value })).unwrap_or(Value::Null),
        "userinfo": {
            "user": {
                "uin": activity.sender_uin,
                "nickname": activity.sender_name
            }
        },
        "meta": {
            "source": "feeds2",
            "timeText": activity.time_text
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommends_offset_by_target_year() {
        assert_eq!(recommend_max_offset(0), 0);
        assert_eq!(recommend_max_offset(2017), 25_000);
        assert_eq!(recommend_max_offset(2015), 50_000);
        assert_eq!(recommend_max_offset(2008), 150_000);
        assert_eq!(recommend_max_offset(2026), 1_500);
    }

    #[test]
    fn parses_supplement_checkpoint() {
        assert_eq!(
            parse_supplement_checkpoint("feeds2:offset=1906"),
            Some(1906)
        );
        assert!(parse_supplement_checkpoint("att=foo").is_none());
    }

    #[test]
    fn extracts_html_from_jsonp_payload() {
        let raw = r#"html:'<li class="f-single f-s-s"><span class="state">赞了我的说说</span></li>',is_public_pav"#;
        let html = extract_h5_feeds_html(raw).unwrap();
        assert!(html.contains("f-single"));
    }

    #[test]
    fn maps_like_activity_to_feed_shape() {
        let activity = ParsedActivity {
            sender_uin: "123".into(),
            sender_name: "Alice".into(),
            content: "今天很好".into(),
            time_text: "昨天 12:30".into(),
            event_time: 1_700_000_000,
            image_urls: vec![],
            kind: ActivityKind::Like,
        };
        let feed = activity_to_feed_value(&activity, "999");
        assert_eq!(
            feed.pointer("/comm/subid").and_then(Value::as_i64),
            Some(217)
        );
        assert!(feed
            .pointer("/original/cell_id/cellid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .starts_with("feeds2:"));
    }
}
