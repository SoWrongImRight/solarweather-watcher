use chrono::{DateTime, Duration, TimeZone, Utc, Datelike};
use chrono::Timelike;
use chrono_tz::Tz;
use regex::Regex;
use reqwest::Client;
use serde_json::Value;
use std::{env, time::Duration as StdDuration};
use tokio::time::{interval, sleep};

// Bring the async transport trait into scope so `.send().await` compiles.
use lettre::AsyncTransport;

// NOAA endpoints
const KP_URL: &str = "https://services.swpc.noaa.gov/products/noaa-planetary-k-index-forecast.json";
const ALERTS_URL: &str = "https://services.swpc.noaa.gov/products/alerts.json";
const BZ_URL: &str = "https://services.swpc.noaa.gov/json/rtsw/rtsw_mag_1m.json";
const SPD_URL: &str = "https://services.swpc.noaa.gov/json/rtsw/rtsw_speed_1m.json";

#[derive(Clone)]
struct Config {
    lat: f64,
    lon: f64,
    tz: Tz,
    // thresholds
    lis_threshold: u8,
    g_min_notify: u8,
    r_min_notify: u8,
    s_min_notify: u8,
    short_bz_nt: f64,
    short_spd_kms: f64,
    // daily report hour (local)
    daily_hour: u32,
    // Email
    smtp_server: Option<String>,
    smtp_port: Option<u16>,       // "587" for STARTTLS, "465" for implicit
    smtp_tls: Option<String>,     // "starttls" (default) or "implicit"
    smtp_user: Option<String>,
    smtp_pass: Option<String>,
    email_from: Option<String>,
    email_to: Option<String>,
    // Twilio
    twilio_sid: Option<String>,
    twilio_token: Option<String>,
    twilio_from: Option<String>,
    sms_to: Option<String>,
}

impl Config {
    fn from_env() -> Self {
        let tz: Tz = env::var("LOCAL_TZ")
            .unwrap_or_else(|_| "America/New_York".to_string())
            .parse()
            .unwrap_or(chrono_tz::America::New_York);

        Self {
            lat: env::var("LAT").ok().and_then(|v| v.parse().ok()).unwrap_or(28.9),
            lon: env::var("LON").ok().and_then(|v| v.parse().ok()).unwrap_or(-81.3),
            tz,
            lis_threshold: env::var("LIS_THRESHOLD").ok().and_then(|v| v.parse().ok()).unwrap_or(40),
            g_min_notify: env::var("G_MIN_NOTIFY").ok().and_then(|v| v.parse().ok()).unwrap_or(2),
            r_min_notify: env::var("R_MIN_NOTIFY").ok().and_then(|v| v.parse().ok()).unwrap_or(2),
            s_min_notify: env::var("S_MIN_NOTIFY").ok().and_then(|v| v.parse().ok()).unwrap_or(2),
            short_bz_nt: env::var("SHORT_BZ_NT").ok().and_then(|v| v.parse().ok()).unwrap_or(-10.0),
            short_spd_kms: env::var("SHORT_SPD_KMS").ok().and_then(|v| v.parse().ok()).unwrap_or(600.0),
            daily_hour: env::var("DAILY_REPORT_HOUR").ok().and_then(|v| v.parse().ok()).unwrap_or(7),
            smtp_server: env::var("SMTP_SERVER").ok(),
            smtp_port: env::var("SMTP_PORT").ok().and_then(|v| v.parse().ok()),
            smtp_tls: env::var("SMTP_TLS").ok(),
            smtp_user: env::var("SMTP_USERNAME").ok(),
            smtp_pass: env::var("SMTP_PASSWORD").ok(),
            email_from: env::var("EMAIL_FROM").ok(),
            email_to: env::var("EMAIL_TO").ok(),
            twilio_sid: env::var("TWILIO_ACCOUNT_SID").ok(),
            twilio_token: env::var("TWILIO_AUTH_TOKEN").ok(),
            twilio_from: env::var("TWILIO_FROM").ok(),
            sms_to: env::var("SMS_TO").ok(),
        }
    }
    fn want_email(&self) -> bool {
        self.smtp_server.is_some() && self.smtp_user.is_some() && self.smtp_pass.is_some()
            && self.email_from.is_some() && self.email_to.is_some()
    }
    fn want_sms(&self) -> bool {
        self.twilio_sid.is_some() && self.twilio_token.is_some()
            && self.twilio_from.is_some() && self.sms_to.is_some()
    }
}

#[tokio::main]
async fn main() {
    let cfg = Config::from_env();
    let client = Client::builder().user_agent("spaceweather-watcher/0.2").build().unwrap();

    // 1) Startup baseline report
    match build_full_status(&client, &cfg).await {
        Ok((lis, level, text)) => {
            let subject = format!("Space Weather Startup Baseline: {} (LIS {})", level, lis.round());
            let _ = send_notifications(&cfg, &subject, &text).await;
            println!("Startup baseline sent: {}", subject);
        }
        Err(e) => eprintln!("Startup baseline error: {e}"),
    }

    // 2) Launch periodic tasks with different cadences
    {
        let cfg_clone = cfg.clone();
        let client_clone = client.clone();
        tokio::spawn(async move { poll_rtsw_task(client_clone, cfg_clone).await }); // 60s
    }
    {
        let cfg_clone = cfg.clone();
        let client_clone = client.clone();
        tokio::spawn(async move { poll_alerts_task(client_clone, cfg_clone).await }); // 5m
    }
    {
        let cfg_clone = cfg.clone();
        let client_clone = client.clone();
        tokio::spawn(async move { poll_kp_task(client_clone, cfg_clone).await }); // 30m
    }

    // 3) Daily 7AM local outlook
    daily_report_scheduler(&client, &cfg).await;
}

// ---------- Schedulers ----------
async fn poll_rtsw_task(client: Client, cfg: Config) {
    let mut last_short_sent: i64 = 0;
    let mut last_lis_sent: i64 = 0;
    let mut intv = interval(StdDuration::from_secs(60));
    loop {
        intv.tick().await;
        if let Ok((lis, level, text, short_flag)) = build_quick_status(&client, &cfg).await {
            let now = Utc::now().timestamp();
            let can_send_short = now - last_short_sent >= 10 * 60; // 10 min cooldown
            let can_send_lis = now - last_lis_sent >= 30 * 60;     // 30 min cooldown
            if short_flag && can_send_short {
                let subject = format!("Space Weather: {} (LIS {})", level, lis.round());
                let _ = send_notifications(&cfg, &subject, &text).await;
                println!("Short-fuse warn sent: {}", subject);
                last_short_sent = now;
            } else if lis >= cfg.lis_threshold as f64 && can_send_lis {
                let subject = format!("Space Weather: {} (LIS {})", level, lis.round());
                let _ = send_notifications(&cfg, &subject, &text).await;
                println!("LIS warn sent: {}", subject);
                last_lis_sent = now;
            }
        }
    }
}

async fn poll_alerts_task(client: Client, cfg: Config) {
    let mut intv = interval(StdDuration::from_secs(300)); // 5 min
    let mut last_levels: (u8, u8, u8) = (0, 0, 0);
    loop {
        intv.tick().await;
        if let Ok((g, r, s)) = fetch_alert_levels(&client).await {
            if g >= cfg.g_min_notify || r >= cfg.r_min_notify || s >= cfg.s_min_notify {
                if (g, r, s) != last_levels {
                    let (_lis, _lvl, body) = summarize_for_email(&client, &cfg)
                        .await
                        .unwrap_or((0.0, "Low".into(), "".into()));
                    let subject = format!("SWPC Alerts: G{} R{} S{}", g, r, s);
                    let _ = send_notifications(&cfg, &subject, &body).await;
                    println!("Alert-level change sent: {}", subject);
                    last_levels = (g, r, s);
                }
            }
        }
    }
}

async fn poll_kp_task(client: Client, _cfg: Config) {
    let mut intv = interval(StdDuration::from_secs(1800)); // 30 min
    loop {
        intv.tick().await;
        let _ = fetch_kp_max24(&client).await; // keep outlook warm
    }
}

async fn daily_report_scheduler(client: &Client, cfg: &Config) {
    loop {
        let now_local: DateTime<Tz> = Utc::now().with_timezone(&cfg.tz);
        let today = now_local.date_naive();
        let today_run = cfg
            .tz
            .with_ymd_and_hms(today.year(), today.month(), today.day(), cfg.daily_hour, 0, 0)
            .unwrap();
        let target_local = if now_local >= today_run {
            today_run + Duration::days(1)
        } else {
            today_run
        };
        let sleep_for = target_local.with_timezone(&Utc) - Utc::now();
        let dur = sleep_for.to_std().unwrap_or(StdDuration::from_secs(0));
        println!("Next daily report at {}", target_local);
        sleep(dur).await;

        if let Ok((_lis, _lvl, text)) = build_full_status(client, cfg).await {
            let subject = format!(
                "Daily Space Weather Outlook — {}",
                target_local.format("%Y-%m-%d")
            );
            let _ = send_notifications(cfg, &subject, &text).await;
            println!("Daily report sent: {}", subject);
        }
        sleep(StdDuration::from_secs(24 * 3600)).await;
    }
}

// ---------- Status builders ----------
async fn build_quick_status(
    client: &Client,
    cfg: &Config,
) -> Result<(f64, String, String, bool), String> {
    let (kp, bz, spd) = tokio::try_join!(
        fetch_kp_max24(client),
        fetch_latest_value(client, BZ_URL, "bz_gsm"),
        fetch_latest_value(client, SPD_URL, "speed")
    )
    .map_err(|e| e.to_string())?;

    let (g, r, s) = fetch_alert_levels(client).await.map_err(|e| e.to_string())?;
    let daylight = is_daylight_local(Utc::now(), cfg.tz, 7, 19);

    let (lis, level, _diag, short) = score_local(
        cfg.lat,
        kp,
        bz,
        spd,
        g,
        r,
        s,
        daylight,
        cfg.short_bz_nt,
        cfg.short_spd_kms,
    );

    let body = format_report(cfg, lis, &level, kp, bz, spd, g, r, s, daylight);
    Ok((lis, level, body, short))
}

async fn build_full_status(client: &Client, cfg: &Config) -> Result<(f64, String, String), String> {
    let (lis, level, body, _short) = build_quick_status(client, cfg).await?;
    Ok((lis, level, body))
}

async fn summarize_for_email(
    client: &Client,
    cfg: &Config,
) -> Result<(f64, String, String), String> {
    build_full_status(client, cfg).await
}

fn format_report(
    cfg: &Config,
    lis: f64,
    level: &str,
    kp: f64,
    bz: Option<f64>,
    spd: Option<f64>,
    g: u8,
    r: u8,
    s: u8,
    daylight: bool,
) -> String {
    let now_local: DateTime<Tz> = Utc::now().with_timezone(&cfg.tz);
    format!(
        "Space Weather Status — {}\n\nLocal Impact Score: {} ({})\n\nInputs:\n  • Kp (max next 24h): {:.1}\n  • L1 Bz: {:?} nT\n  • L1 Speed: {:?} km/s\n  • Alerts — G:{}  R:{}  S:{}\n  • Daylight now: {}\n\nGuidance:\n  • LIS ≥ {} triggers warnings (configurable).\n  • Short-fuse trigger: Bz ≤ {} nT & Speed ≥ {} km/s (≈15–60 min lead).\n",
        now_local.format("%Y-%m-%d %H:%M %Z"),
        lis.round(),
        level,
        kp,
        bz,
        spd,
        g,
        r,
        s,
        daylight,
        cfg.lis_threshold,
        cfg.short_bz_nt,
        cfg.short_spd_kms
    )
}

// ---------- Fetchers ----------
async fn fetch_kp_max24(client: &Client) -> Result<f64, reqwest::Error> {
    let txt = client.get(KP_URL).send().await?.text().await?;
    let v: Value = serde_json::from_str(&txt).unwrap_or(Value::Null);

    let now = Utc::now();
    let end = now + Duration::hours(24);
    let mut max_kp = 0.0;

    if let Value::Array(rows) = v {
        let header = if let Some(Value::Array(first)) = rows.get(0) {
            first.get(0).and_then(|x| x.as_str()) == Some("time_tag")
        } else {
            false
        };
        let start_idx = if header { 1 } else { 0 };

        for row in rows.iter().skip(start_idx) {
            if let Value::Array(cols) = row {
                if cols.len() >= 3 {
                    if let (Some(ts), Some(kps)) =
                        (cols.get(0).and_then(|x| x.as_str()), cols.get(2).and_then(|x| x.as_str()))
                    {
                        if let Ok(t) =
                            chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S")
                        {
                            let t = t.and_utc();
                            if t >= now && t <= end {
                                if let Ok(kp) = kps.parse::<f64>() {
                                    if kp > max_kp {
                                        max_kp = kp;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(max_kp)
}

async fn fetch_latest_value(
    client: &Client,
    url: &str,
    key: &str,
) -> Result<Option<f64>, reqwest::Error> {
    let v: Value = client.get(url).send().await?.json().await.unwrap_or(Value::Null);
    if let Value::Array(rows) = v {
        for row in rows.iter().rev() {
            if let Value::Object(map) = row {
                if let Some(val) = map.get(key) {
                    if let Some(n) = val.as_f64() {
                        return Ok(Some(n));
                    }
                    if let Some(s) = val.as_str() {
                        if let Ok(n) = s.parse::<f64>() {
                            return Ok(Some(n));
                        }
                    }
                }
            }
        }
    }
    Ok(None)
}

async fn fetch_alert_levels(client: &Client) -> Result<(u8, u8, u8), reqwest::Error> {
    let v: Value = client.get(ALERTS_URL).send().await?.json().await.unwrap_or(Value::Null);
    let (mut g, mut r, mut s) = (0u8, 0u8, 0u8);

    let re_g = Regex::new(r"G([1-5])").unwrap();
    let re_r = Regex::new(r"R([1-5])").unwrap();
    let re_s = Regex::new(r"S([1-5])").unwrap();

    if let Value::Array(arr) = v {
        for item in arr {
            if let Some(msg) = item.get("message").and_then(|m| m.as_str()) {
                let m = msg.to_uppercase();
                for cap in re_g.captures_iter(&m) {
                    if let Some(d) = cap.get(1) {
                        g = g.max(d.as_str().parse::<u8>().unwrap_or(0));
                    }
                }
                for cap in re_r.captures_iter(&m) {
                    if let Some(d) = cap.get(1) {
                        r = r.max(d.as_str().parse::<u8>().unwrap_or(0));
                    }
                }
                for cap in re_s.captures_iter(&m) {
                    if let Some(d) = cap.get(1) {
                        s = s.max(d.as_str().parse::<u8>().unwrap_or(0));
                    }
                }
            }
        }
    }
    Ok((g, r, s))
}

// ---------- Scoring ----------
struct Diag {
    _geo_weight: f64,
}

fn score_local(
    lat: f64,
    kp_max24: f64,
    bz: Option<f64>,
    spd: Option<f64>,
    g: u8,
    r: u8,
    s: u8,
    daylight: bool,
    short_bz_nt: f64,
    short_spd_kms: f64,
) -> (f64, String, Diag, bool) {
    // Geomagnetic (Kp) weighting by latitude: floor 0.2 below ~30°, -> 1.0 by ~50°
    let geo_w = ((lat.abs() - 30.0) / 20.0).clamp(0.2, 1.0);
    // Kp 4 -> 0, Kp 9 -> 1
    let geo_base = ((kp_max24 - 4.0) / 5.0).clamp(0.0, 1.0);
    let geo_score = 60.0 * geo_w * geo_base;

    // Short-fuse trigger (L1)
    let mut short: f64 = 0.0;
    let mut short_flag = false;
    if let (Some(bzv), Some(spdv)) = (bz, spd) {
        if bzv <= short_bz_nt && spdv >= short_spd_kms {
            short += 20.0;
            short_flag = true;
        }
        if bzv <= short_bz_nt - 5.0 {
            short += 5.0;
        }
        if spdv >= short_spd_kms + 200.0 {
            short += 5.0;
        }
    }
    let short_score = short.clamp(0.0_f64, 30.0_f64);

    // Radio blackout R-scale; stronger effect during daytime
    let r_map = [0.0, 6.0, 12.0, 18.0, 22.0, 25.0];
    let mut r_score = *r_map.get(r as usize).unwrap_or(&25.0);
    if !daylight {
        r_score *= 0.35;
    }

    // Radiation S-scale; smaller effect at low latitudes
    let s_map = [0.0, 2.0, 5.0, 8.0, 10.0, 10.0];
    let base_s = *s_map.get(s as usize).unwrap_or(&10.0);
    let s_score = if lat.abs() < 40.0 { base_s * 0.6 } else { base_s };

    let lis = (geo_score + short_score + r_score + s_score).clamp(0.0, 100.0);
    let level = if lis >= 80.0 {
        "Severe"
    } else if lis >= 60.0 {
        "High"
    } else if lis >= 40.0 {
        "Moderate"
    } else if lis >= 20.0 {
        "Elevated"
    } else {
        "Low"
    }
    .to_string();

    (
        lis,
        level,
        Diag {
            _geo_weight: (geo_w * 100.0).round() as f64 / 100.0,
        },
        short_flag,
    )
}

fn is_daylight_local(now_utc: DateTime<Utc>, tz: Tz, start_h: u32, end_h: u32) -> bool {
    let local = now_utc.with_timezone(&tz);
    let h = local.hour();
    h >= start_h && h < end_h
}

// ---------- Notifications ----------
async fn send_notifications(cfg: &Config, subject: &str, body: &str) {
    if cfg.want_email() {
        if let Err(e) = send_email(cfg, subject, body).await {
            eprintln!("Email send error: {e}");
        }
    }
    if cfg.want_sms() {
        if let Err(e) = send_sms_twilio(cfg, &format!("{subject}\n{body}")).await {
            eprintln!("SMS send error: {e}");
        }
    }
}

// Build SMTP transport with selectable TLS mode/port.
fn build_mailer(
    cfg: &Config,
) -> Result<lettre::AsyncSmtpTransport<lettre::Tokio1Executor>, Box<dyn std::error::Error>> {
    use lettre::{transport::smtp::authentication::Credentials, AsyncSmtpTransport, Tokio1Executor};

    let server = cfg.smtp_server.clone().ok_or("SMTP_SERVER missing")?;
    let creds = Credentials::new(
        cfg.smtp_user.clone().ok_or("SMTP_USERNAME missing")?,
        cfg.smtp_pass.clone().ok_or("SMTP_PASSWORD missing")?,
    );

    let tls = cfg.smtp_tls.clone().unwrap_or_else(|| "starttls".to_string());
    let port = cfg.smtp_port.unwrap_or(if tls == "implicit" { 465 } else { 587 });

    let mailer = if tls == "implicit" {
        // Implicit TLS (usually port 465)
        AsyncSmtpTransport::<Tokio1Executor>::relay(&server)?
            .port(port)
            .credentials(creds)
            .build()
    } else {
        // STARTTLS (usually port 587)
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&server)?
            .port(port)
            .credentials(creds)
            .build()
    };
    Ok(mailer)
}

async fn send_email(
    cfg: &Config,
    subject: &str,
    body: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use lettre::message::{header, Mailbox, Message, MultiPart, SinglePart};

    let from = cfg.email_from.clone().ok_or("EMAIL_FROM missing")?;
    let to = cfg.email_to.clone().ok_or("EMAIL_TO missing")?;

    let msg = Message::builder()
        .from(from.parse::<Mailbox>()?)
        .to(to.parse::<Mailbox>()?)
        .subject(subject)
        .multipart(
            MultiPart::alternative().singlepart(
                SinglePart::builder()
                    .header(header::ContentType::TEXT_PLAIN)
                    .body(body.to_string()),
            ),
        )?;

    let mailer = build_mailer(cfg)?;
    mailer
        .send(msg)
        .await
        .map(|_| ())
        .map_err(|e| format!("SMTP send failed: {e:?}").into())
}

async fn send_sms_twilio(cfg: &Config, body: &str) -> Result<(), Box<dyn std::error::Error>> {
    let sid = cfg.twilio_sid.clone().unwrap();
    let token = cfg.twilio_token.clone().unwrap();
    let from = cfg.twilio_from.clone().unwrap();
    let to = cfg.sms_to.clone().unwrap();

    let url = format!(
        "https://api.twilio.com/2010-04-01/Accounts/{}/Messages.json",
        sid
    );

    let client = Client::new();
    let params = [("From", from.as_str()), ("To", to.as_str()), ("Body", body)];

    let resp = client
        .post(url)
        .basic_auth(sid, Some(token))
        .form(&params)
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        // Only consume body on error; otherwise we'd move `resp`.
        let t = resp.text().await.unwrap_or_default();
        Err(format!("Twilio API error: {} {}", status, t).into())
    } else {
        Ok(())
    }
}
