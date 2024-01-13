use anyhow::Context;
use chrono::{DateTime, Utc};
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Deserializer, Serialize};
use sha256::digest;
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::ops::Bound;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tokio::io::AsyncWriteExt;
use tokio_util::codec::{BytesCodec, FramedRead};
use walkdir::WalkDir;

const MAX_SEGMENT_LENGTH: f64 = 3300.0;
const CONCURRENT_TRANSCRIBES: usize = 4;
const MAX_PRICE: f64 = 30.0;
const PRICE_PER_SECOND: f64 = 0.000193;

#[derive(Debug, Serialize, Deserialize)]
struct StoringFields(HashMap<String, serde_json::Value>);

#[derive(Deserialize)]
struct Config {
    google_api_key: String,
    gladia_api_key: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct YouTubeChannelResponse<I> {
    items: Vec<I>,
    next_page_token: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct YouTubeSearchItem {
    id: YouTubeId,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct YouTubeListItem {
    id: String,
    snippet: YouTubeVideoSnippet,
    recording_details: YouTubeRecordingDetails,
    content_details: YouTubeContentDetails,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct YouTubeId {
    video_id: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct YouTubeVideoSnippet {
    title: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct YouTubeRecordingDetails {
    recording_date: DateTime<Utc>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct YouTubeContentDetails {
    // TODO: https://github.com/chronotope/chrono/issues/579
    #[serde(deserialize_with = "iso8601dur")]
    duration: chrono::Duration,
}

fn iso8601dur<'de, D>(deser: D) -> Result<chrono::Duration, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deser)?;
    let mut dur = chrono::Duration::zero();
    let Some(mut rest) = s.strip_prefix("P") else {
        return Err(serde::de::Error::invalid_value(
            serde::de::Unexpected::Str(&s),
            &"'P'",
        ));
    };

    let mut saw_t = false;
    while !rest.is_empty() {
        let next_non_digit = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.' && c != ',')
            .unwrap_or(rest.len());
        let value = &rest[..next_non_digit];
        rest = &rest[next_non_digit..];
        let unit = rest.chars().next().ok_or_else(|| {
            serde::de::Error::invalid_value(
                serde::de::Unexpected::Str(""),
                &"a unit for the preceeding number",
            )
        })?;
        rest = &rest[unit.len_utf8()..];
        if unit == 'T' {
            saw_t = true;
            continue;
        }
        let value: f64 = value.parse().map_err(|_| {
            serde::de::Error::invalid_value(
                serde::de::Unexpected::Str(&rest[..next_non_digit]),
                &"a number",
            )
        })?;
        match unit {
            'H' => {
                dur = dur + chrono::Duration::hours(value.round() as i64);
            }
            'M' if saw_t => {
                dur = dur + chrono::Duration::minutes(value.round() as i64);
            }
            'M' => {
                dur = dur + chrono::Duration::days((30.0 * value).round() as i64);
            }
            'S' => {
                dur = dur + chrono::Duration::seconds(value.round() as i64);
            }
            'D' => {
                dur = dur + chrono::Duration::days(value.round() as i64);
            }
            'W' => {
                dur = dur + chrono::Duration::weeks(value.round() as i64);
            }
            'Y' => {
                dur = dur + chrono::Duration::days((365.0 * value).round() as i64);
            }
            c if saw_t => {
                return Err(serde::de::Error::invalid_value(
                    serde::de::Unexpected::Char(c),
                    &"H, M, or S",
                ))
            }
            c => {
                return Err(serde::de::Error::invalid_value(
                    serde::de::Unexpected::Char(c),
                    &"Y, M, W, D, or T",
                ))
            }
        }
    }
    Ok(dur)
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
struct GladiaTranscribeResponse {
    prediction: Vec<Prediction>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
struct Prediction {
    // confidence: f64,
    // language: String,
    time_begin: f64,
    time_end: f64,
    transcription: String,
    // NOTE: ignoring words
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct YouTubeCaptionPost {
    snippet: YouTubeCaptionSnippet,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct YouTubeCaptionSnippet {
    video_id: String,
    language: String,
    name: String,
    is_draft: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LocalVideo {
    // NOTE: the order of the fields matter for Ord here
    length: Duration,
    recorded: DateTime<Utc>,
    path: PathBuf,
    delay: Duration,
}

#[derive(Debug, Clone)]
struct Video {
    youtube: YouTubeListItem,
    local: LocalVideo,
}

async fn with_cache<F, FF>(url: &str, on_miss: F) -> anyhow::Result<serde_json::Value>
where
    F: FnOnce() -> FF,
    FF: Future<Output = anyhow::Result<serde_json::Value>>,
{
    let mut url_file = digest(url);
    url_file.push_str(".json");
    if tokio::fs::try_exists(&url_file).await.unwrap_or(false) {
        eprintln!(" .. satisfied with cache hit");
        let json = tokio::fs::read(&url_file)
            .await
            .with_context(|| format!("read json cache from '{url_file}'"))?;
        Ok(serde_json::from_slice(&json)
            .with_context(|| format!("parse json cache from '{url_file}'"))?)
    } else {
        eprintln!(" .. cache miss");
        let res = on_miss().await.context("issue request on miss")?;
        tokio::fs::write(
            &url_file,
            serde_json::to_vec(&res).context("serialize json")?,
        )
        .await
        .context("write out json cache")?;
        Ok(res)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = tokio::fs::read_to_string("config.toml")
        .await
        .context("read config")?;
    let config: Config = toml::from_str(&config).context("parse config")?;

    let client = reqwest::Client::new();
    let mut next_page_token = None;

    println!("==> finding video files locally");
    let mut local_videos = BTreeSet::new();
    for entry in WalkDir::new(
        std::env::args()
            .nth(1)
            .expect("no path to video files given"),
    )
    .same_file_system(true)
    {
        let entry = entry.context("walk dir")?;
        let Some(name) = entry.path().file_name() else {
            continue;
        };
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some((name, ext)) = name.split_once('.') else {
            continue;
        };
        let Ok(dt) = name.parse::<DateTime<Utc>>() else {
            continue;
        };
        let src = std::fs::File::open(entry.path()).context("failed to open media")?;
        let mss = MediaSourceStream::new(Box::new(src), Default::default());
        let mut hint = Hint::new();
        hint.with_extension(ext);
        let meta_opts: MetadataOptions = Default::default();
        let fmt_opts: FormatOptions = Default::default();
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &fmt_opts, &meta_opts)
            .context("unsupported format")?;
        let Some(track) = probed
            .format
            .tracks()
            .iter()
            .find(|t| t.codec_params.channel_layout.is_some())
        else {
            continue;
        };
        let (Some(time_base), Some(n_frames)) =
            (track.codec_params.time_base, track.codec_params.n_frames)
        else {
            continue;
        };
        let length = time_base.calc_time(n_frames);
        let length = Duration::from_secs(length.seconds) + Duration::from_secs_f64(length.frac);
        // TODO: for whatever reason, track.codec_params.start_ts is always 0, so use ffprobe
        let delay = tokio::process::Command::new("ffprobe")
            .arg("-i")
            .arg(entry.path())
            .arg("-show_entries")
            .arg("stream=start_time")
            .arg("-select_streams")
            .arg("a")
            .arg("-hide_banner")
            .arg("-of")
            .arg("default=noprint_wrappers=1:nokey=1")
            .output()
            .await
            .with_context(|| format!("ffprobe '{}'", entry.path().display()))?;
        let delay = std::str::from_utf8(&delay.stdout)
            .with_context(|| format!("non-utf8 in ffprobe '{}'", entry.path().display()))?;
        let delay: f64 = delay.trim().parse().with_context(|| {
            format!(
                "bad delay float in ffprobe '{}': {delay}",
                entry.path().display()
            )
        })?;
        let delay = if delay.is_sign_negative() {
            anyhow::ensure!(
                delay.abs() < 0.05,
                "very negative audio delay, {}, in '{}'",
                delay,
                entry.path().display()
            );
            Duration::default()
        } else {
            Duration::from_secs_f64(delay)
        };
        local_videos.insert(LocalVideo {
            recorded: dt,
            length,
            path: entry.path().to_path_buf(),
            delay,
        });
    }
    println!(" -> found {} videos", local_videos.len());

    // let channel = "jonhoo";
    let channel = "UC_iD0xppBwwsrM9DegC5cQQ";
    let mut ytvids = Vec::new();
    println!("==> finding long, uncaptioned videos from YouTube");
    loop {
        let url = format!(
            "https://youtube.googleapis.com/youtube/v3/search?part=snippet&channelId={}&key={}&order=date&maxResults=100&type=video&fields=nextPageToken,items(id)&videoDuration=long&videoCaption=none{}",
            channel, config.google_api_key,
            next_page_token.as_ref().map(|pt| format!("&pageToken={pt}")).unwrap_or_default()
        );
        let json = with_cache(&url, || async {
            let res = client
                .get(&url)
                .header("Accept", "application/json")
                .send()
                .await
                .context("issue search query")?;

            let res: serde_json::Value = res.json().await.context("parse search result json")?;
            Ok(res)
        })
        .await
        .context("video search")?;
        let res: YouTubeChannelResponse<YouTubeSearchItem> =
            serde_json::from_value(json).context("type json")?;
        println!(" -> found page of {} videos", res.items.len());
        for video in res.items {
            ytvids.push(video);
        }

        next_page_token = res.next_page_token;
        if next_page_token.is_none() {
            break;
        }
    }
    println!(" -> found {} videos total", ytvids.len());

    let mut youtube_videos = Vec::new();
    println!("==> extracting additional video information");
    for chunk in ytvids.chunks(50) {
        let ids =
            chunk
                .iter()
                .map(|video| &video.id.video_id)
                .fold(String::new(), |mut accum, id| {
                    if accum.is_empty() {
                        String::from(id)
                    } else {
                        accum.push(',');
                        accum.push_str(id);
                        accum
                    }
                });
        let url = format!(
                "https://www.googleapis.com/youtube/v3/videos?part=snippet,recordingDetails,contentDetails&fields=items(id,snippet(title),recordingDetails(recordingDate),contentDetails(duration))&id={}&maxResults=50&key={}",
                ids,
                config.google_api_key,
            );
        let res = with_cache(&url, || async {
            let res = client
                .get(&url)
                .header("Accept", "application/json")
                .send()
                .await
                .context("issue video list query")?;

            let res: serde_json::Value = res.json().await.context("parse video list json")?;
            Ok(res)
        })
        .await
        .context("grab video details")?;
        let res: YouTubeChannelResponse<YouTubeListItem> =
            serde_json::from_value(res).context("type json")?;
        println!(" -> found page of {} videos", res.items.len());
        for video in res.items {
            youtube_videos.push(video);
        }
    }
    youtube_videos.sort_by(|a, b| {
        a.recording_details
            .recording_date
            .cmp(&b.recording_details.recording_date)
            .then(a.id.cmp(&b.id))
    });

    println!("==> matching local files to YouTube videos");
    let mut videos: Vec<_> = youtube_videos
        .into_iter()
        .filter_map(|meta| {
            let id = &meta.id;
            let recorded = meta.recording_details.recording_date;
            let length = meta
                .content_details
                .duration
                .to_std()
                .expect("no negative durations");
            let cursor = local_videos.range((
                Bound::Included(LocalVideo {
                    length: length.saturating_sub(Duration::from_secs(7)),
                    recorded: DateTime::default(),
                    path: PathBuf::new(),
                    delay: Duration::default(),
                }),
                Bound::Unbounded,
            ));
            let mut best: Option<&LocalVideo> = None;
            for candidate in cursor {
                if candidate.length > length + Duration::from_secs(7) {
                    break;
                }
                // the metadata from youtube is _really_ just a date, the time part is always
                // 00:00:00. imagine it was _really_ recorded late evening PST, then the timestamp
                // on the file might be +1D UTC. which would mean the difference to 00:00:00 is
                // >24H. so, we just check if the date in one is within 1D of the date in the
                // other.
                if (candidate.recorded.date_naive() - recorded.date_naive()).num_days() > 1 {
                    continue;
                }
                best = Some(if let Some(prev_best) = best.take() {
                    let prev_diff =
                        prev_best.length.as_secs() as i64 - candidate.length.as_secs() as i64;
                    let this_diff =
                        prev_best.length.as_secs() as i64 - candidate.length.as_secs() as i64;
                    if prev_diff.abs() < this_diff.abs() {
                        prev_best
                    } else {
                        candidate
                    }
                } else {
                    candidate
                });
            }

            if let Some(v) = best {
                println!(
                    " -> {} is probably \"{}\" ({}; recorded: {}, {:?} vs {:?})",
                    v.path
                        .strip_prefix("/mnt/nas/recordings/")
                        .expect("this is the base path we passed in")
                        .display(),
                    meta.snippet.title,
                    id,
                    meta.recording_details.recording_date,
                    v.length,
                    meta.content_details
                        .duration
                        .to_std()
                        .expect("no negative durations")
                );
                if !v.delay.is_zero() {
                    println!(" .. audio delay is {:?}", v.delay);
                }
                Some(Video {
                    youtube: meta,
                    local: v.clone(),
                })
            } else {
                eprintln!(
                    "no match for {id} from {} with length {:?}",
                    recorded, length
                );
                None
            }
        })
        .collect();
    videos.sort_by_key(|v| v.youtube.content_details.duration);

    println!("==> transcribing videos with Gladia");
    let s = Arc::new(tokio::sync::Semaphore::new(CONCURRENT_TRANSCRIBES));
    let cost_in_cents = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut tasks = tokio::task::JoinSet::new();
    for video in videos {
        let s = Arc::clone(&s);
        let cost_in_cents = Arc::clone(&cost_in_cents);
        let id = video.youtube.id.clone();
        let client = client.clone();
        let gladia_api_key = config.gladia_api_key.clone();
        let fut = async move {
            let _permit = s.acquire().await;

            let srt = format!(
                "{}-{}-{}.srt",
                video.youtube.recording_details.recording_date.date_naive(),
                video.youtube.id,
                video.youtube.snippet.title.replace('/', "-"),
            );
            if tokio::fs::try_exists(&srt)
                .await
                .context("check for existence")?
            {
                println!(
                    " -> not transcribing {} (.srt already exists)",
                    video.youtube.snippet.title
                );
                return Ok(());
            }

            let this_in_cents =
                (100.0 * PRICE_PER_SECOND * video.local.length.as_secs_f64()).round() as u64;
            let total_cost =
                cost_in_cents.fetch_add(this_in_cents, Ordering::AcqRel) as f64 / 100.0;
            if total_cost > MAX_PRICE {
                // decrement again in case a shorter video comes along
                cost_in_cents.fetch_sub(this_in_cents, Ordering::AcqRel);
                println!(
                    " -> not transcribing {} (would exceed cost limit)",
                    video.youtube.snippet.title
                );
                return Ok(());
            }

            println!(
                " -> transcribing {} ({} <> {})",
                video.youtube.snippet.title,
                video.youtube.id,
                video
                    .local
                    .path
                    .strip_prefix("/mnt/nas/recordings/")
                    .expect("this is the base path we passed in")
                    .display(),
            );

            // https://gladia-stt.nolt.io/23
            // https://gladia-stt.nolt.io/24
            let nsplits = (video.local.length.as_secs_f64() / MAX_SEGMENT_LENGTH).ceil() as u64;
            let mut start = Duration::default();
            let dur = video.local.length.as_secs() / nsplits;
            let mut captions = Vec::new();
            for split in 0..nsplits {
                let t = if split < nsplits - 1 {
                    Some(if split == 0 {
                        dur - video.local.delay.as_secs()
                    } else {
                        dur
                    })
                } else {
                    None
                };

                let mut ffmpeg = tokio::process::Command::new("ffmpeg");

                // NOTE: we don't use -acodec copy because that would be limited to extracting time
                // segments at block boundaries for the audio codec (e.g., blocks in AAC).
                // instead, we need to reencode, which allows extracting exact times since the
                // input is muxed. it's tempting to reencode to flac, which is lossless, but then
                // we quickly run into the 500MB file size limit. opus would be lovely, but isn't
                // supported (<https://gladia-stt.nolt.io/35>). so, we go with vorbis/ogg. we avoid
                // aac because some aac encoders are bad.
                let start_f64 = start.as_secs_f64();
                let ss = start_f64.to_string();
                ffmpeg
                    .arg("-ss")
                    .arg(ss)
                    .arg("-i")
                    .arg(&video.local.path)
                    .arg("-vn")
                    .arg("-acodec")
                    .arg("libvorbis")
                    .arg("-f")
                    .arg("ogg")
                    .arg("-qscale:a")
                    .arg("8")
                    .arg("-nostdin")
                    .arg("-hide_banner")
                    .arg("-loglevel")
                    .arg("error");

                if !video.local.delay.is_zero() && split == 0 {
                    ffmpeg
                        .arg("-af")
                        .arg(format!("adelay={}", video.local.delay.as_millis()));
                }

                if let Some(t) = t {
                    ffmpeg.arg("-t").arg(t.to_string());
                }

                println!(
                    " .. {} | {} -> {}",
                    video.youtube.id,
                    seconds_to_timestamp(start.as_secs_f64()),
                    seconds_to_timestamp(
                        t.map(|t| start + Duration::from_secs(t))
                            .unwrap_or(video.local.length)
                            .as_secs_f64()
                    )
                );

                let mut ffmpeg = ffmpeg
                    .arg("-")
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .context("ffmpeg split")?;

                let req = client
                    .post("https://api.gladia.io/audio/text/audio-transcription/")
                    .header("x-gladia-key", &gladia_api_key)
                    .header("Accept", "application/json")
                    .multipart(
                        Form::new()
                            .part(
                                "audio",
                                Part::stream(reqwest::Body::wrap_stream(FramedRead::new(
                                    ffmpeg.stdout.take().expect("set to piped"),
                                    BytesCodec::new(),
                                )))
                                .mime_str("application/ogg")
                                .context("valid mime string")?,
                            )
                            .text("toggle_diarization", "false"),
                    )
                    .send();

                let (res, ffmpeg) = tokio::join!(req, ffmpeg.wait_with_output());
                let ffmpeg = ffmpeg.context("extract audio")?;
                let res = match (res, ffmpeg.status.success()) {
                    (Ok(res), true) if res.status().is_success() => res,
                    (Ok(res), _) => {
                        // we got an error response, so print all we can
                        // if res is a 2XX but ffmpeg failed, we'll also land here
                        // but that's probably appropriate
                        let ffmpeg =
                            std::str::from_utf8(&ffmpeg.stderr).expect("ffmpeg stderr is utf-8");
                        let code = res.status();
                        let gladia = res
                            .text()
                            .await
                            .unwrap_or_else(|_| String::from("<failed to read>"));
                        return Err(anyhow::anyhow!(gladia))
                            .with_context(|| format!("HTTP status: {code}"))
                            .with_context(|| format!("ffmpeg output:\n{ffmpeg}"))
                            .context("run transcription");
                    }
                    (Err(e), _) => {
                        // the request couldn't even be issued. probably an I/O error.
                        let ffmpeg =
                            std::str::from_utf8(&ffmpeg.stderr).expect("ffmpeg stderr is utf-8");
                        Err(e)
                            .with_context(|| format!("ffmpeg output:\n{ffmpeg}"))
                            .context("issue transcribe request")?
                    }
                };

                let res: serde_json::Value = res.json().await.context("parse json")?;
                let mut res: GladiaTranscribeResponse = serde_json::from_value(res).unwrap();

                // if we're not at the last segment, we need to find a good place to split
                if split < nsplits - 1 {
                    // here comes the trick
                    // we grabbed captions for [start..start + dur]
                    // _but_ the end point may be in the middle of a caption!
                    // so, we find the time of last "gap"
                    // drop all the captions following that
                    // and resume captioning from that point rather than start
                    let mut next_starts = res
                        .prediction
                        .last()
                        .expect("always at least one caption")
                        .time_end;
                    let mut best_gap: Option<(usize, f64, f64)> = None;
                    for i in 0..res.prediction.len().min(20) {
                        let i = res.prediction.len() - i - 1;
                        let gap = next_starts - res.prediction[i].time_end;
                        // prefer breaking at natural sentence boundaries as it reduces the
                        // likelihood that the next transcription will start at a weird point in a
                        // sentence. the ... and , endings are also acceptable, but slightly less
                        // "good" since we may end up with a captial letter starting the next
                        // caption.
                        let score = if res.prediction[i].transcription.ends_with("...")
                            || res.prediction[i].transcription.ends_with(',')
                        {
                            1.3 * gap
                        } else if res.prediction[i]
                            .transcription
                            .ends_with(|c| c == '.' || c == '?' || c == '!' || c == ':')
                        {
                            2.0 * gap
                        } else {
                            gap
                        };
                        best_gap = Some(if let Some(prev) = best_gap.take() {
                            if score > prev.2 {
                                (i, gap, score)
                            } else {
                                prev
                            }
                        } else {
                            (i, gap, score)
                        });
                        next_starts = res.prediction[i].time_begin;
                    }
                    let gap = best_gap.expect("always a gap");
                    let slice_at = start
                        + Duration::from_secs_f64(res.prediction[gap.0].time_end + gap.1 / 2.0);
                    println!(
                        " .. {} | slicing at {} in {:?} gap after: {}",
                        video.youtube.id,
                        seconds_to_timestamp(slice_at.as_secs_f64()),
                        Duration::from_secs_f64(gap.1),
                        res.prediction[gap.0].transcription
                    );
                    res.prediction.truncate(gap.0 + 1);
                    start = slice_at;
                }

                // whatever captions are left, adjust their start times for the start offset
                captions.extend(res.prediction.into_iter().map(|mut p| {
                    // note: this is specifically start_f64, which is not affected by us updating
                    // start at the end of the gap slicing above.
                    p.time_begin += start_f64;
                    p.time_end += start_f64;
                    p
                }));
            }

            println!(" .. {} | writing .srt", video.youtube.id);
            let mut outfile = tokio::fs::File::create(&srt).await.context("create srt")?;
            for (i, segment) in captions.into_iter().enumerate() {
                let line = format!(
                    "{}{}\n{} --> {}\n{}\n",
                    if i != 0 { "\n" } else { "" },
                    i + 1,
                    seconds_to_timestamp(segment.time_begin),
                    seconds_to_timestamp(segment.time_end),
                    segment.transcription,
                );
                outfile
                    .write_all(line.as_bytes())
                    .await
                    .context("write out srt line")?;
            }
            outfile.flush().await.context("flush srt")?;

            println!(" .. {} | done", video.youtube.id);
            Ok(())
        };
        tasks.spawn(async move {
            fut.await
                .with_context(|| format!("while transcribing {id}"))
        });
    }

    while let Some(v) = tasks.join_next().await {
        let _ = v.context("join failed")?.context("job failed")?;
    }
    eprintln!("==> all transcription completed");

    Ok(())
}

fn seconds_to_timestamp(fracs: f64) -> String {
    let mut is = fracs as i64;
    assert!(is >= 0);
    let h = is / 3600;
    is -= h * 3600;
    let m = is / 60;
    is -= m * 60;
    let s = is;
    let frac = fracs.fract();
    let frac = format!("{:.3}", frac);
    let frac = if let Some(frac) = frac.strip_prefix("0.") {
        format!(",{frac}")
    } else if frac == "1.000" {
        // 0.9995 would be truncated to 1.000 at {:.3}
        String::from(",999")
    } else if frac == "0" {
        // integral number of seconds
        String::from(",000")
    } else {
        unreachable!("bad fractional second: {} -> {frac}", fracs.fract())
    };
    format!("{h:02}:{m:02}:{s:02}{frac}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_ones() {
        assert!(dbg!(seconds_to_timestamp(3661.3)).starts_with("01:01:01,3"));
    }

    #[test]
    fn zero_fract() {
        assert_eq!(seconds_to_timestamp(3661.0), "01:01:01");
    }
}
