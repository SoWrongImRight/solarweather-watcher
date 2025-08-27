# Spaceweather Watcher — Dockerized Rust Service

Location‑aware space‑weather monitor that polls NOAA SWPC APIs, computes a Local Impact Score (LIS) for your latitude, and sends **startup baseline**, **daily 7AM local report**, and **real‑time warnings** by email/SMS.

## Features
- Polls with smart cadences: RTSW (60s), SWPC alerts (5m), Kp forecast (30m)
- Local Impact Score (0–100) with latitude weighting and short‑fuse triggers (Bz/Speed)
- Startup baseline report when the container launches
- Daily 7AM local outlook (configurable)
- Warnings when LIS ≥ threshold or short‑fuse conditions detected
- Email via SMTP (lettre) and SMS via Twilio

## Quick start
```bash
# Build
docker build -t spaceweather-watcher:latest .

# Configure
cp .env.example .env   # edit values

# Run
docker run --env-file .env --name spaceweather --restart unless-stopped -d spaceweather-watcher:latest

# Logs
docker logs -f spaceweather
```

## Environment variables
See `.env.example`. Key knobs:
- `LIS_THRESHOLD` (default 40) – when to warn
- `SHORT_BZ_NT` (default -10) and `SHORT_SPD_KMS` (default 600) – short‑fuse trigger
- `DAILY_REPORT_HOUR` (default 7) and `LOCAL_TZ` (default America/New_York)
- Email creds (`SMTP_*`, `EMAIL_*`) and/or Twilio (`TWILIO_*`, `SMS_TO`)

## Notes
- This depends on public NOAA endpoints. It’s for **pre‑event monitoring** while internet is available.
- For hardened, grid‑down ops, pair with HF/shortwave and magnetometer sources.
