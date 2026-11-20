// Port of digest/index.mjs: `once daily|weekly [--dry-run]` and `serve`.
// Arguments are parsed by hand so the usage line and exit codes match Node exactly (2 usage, 1 failure).
use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use digest_rs::config::{Config, load_config_from_env, secrets_of};
use digest_rs::discord::{self, Digest};
use digest_rs::http;
use digest_rs::js::iso_string;
use digest_rs::mask::LONE_SURROGATE_BASE;
use digest_rs::report::{self, Kind};
use digest_rs::schedule::{self, Spec};

/// Wake hourly and re-check the clock, so suspend/clock jumps cannot skew a run.
const MAX_TIMER_MS: i64 = 3_600_000;
const USAGE: &str = "usage: node digest/index.mjs once daily|weekly [--dry-run] | serve";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Test-only clock override for the Node/Rust parity check (Node ignores it). Only `once` reads it.
fn once_now() -> i64 {
    std::env::var("DIGEST_NOW_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(now_ms)
}

/// `process.stdout.write(`${s}\n`)` / `console.log`. A closed pipe is ignored instead of panicking.
fn out(s: &str) {
    let _ = writeln!(std::io::stdout(), "{s}");
}

fn safe_log(cfg: &Config, msg: &str) {
    let _ = writeln!(std::io::stderr(), "{}", http::scrub(msg, &secrets_of(cfg)));
}

/// Node writes a lone surrogate to UTF-8 stdout as U+FFFD.
fn stdout_text(s: &str) -> String {
    s.chars()
        .map(|c| {
            if (LONE_SURROGATE_BASE..=0xE3FF).contains(&u32::from(c)) {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

async fn run_once(
    client: &reqwest::Client,
    cfg: &Config,
    kind: Kind,
    dry_run: bool,
    now: i64,
) -> Result<(), String> {
    let data = report::collect_all(client, cfg, kind, now, dry_run).await;
    let r = report::build_report(&data, cfg)?;
    let image_names: Vec<String> = r.images.iter().map(|i| i.name.clone()).collect();
    let digest = Digest {
        title: &r.title,
        description: &r.description,
        color: r.color,
        footer: &r.footer,
        fields: &r.fields,
        image_names: &image_names,
    };
    let messages = discord::build_messages(&digest);
    if dry_run {
        out(&discord::to_js_json(&messages, true));
        out("\n--- markdown preview ---\n");
        out(&stdout_text(&report::to_markdown(&r)));
        return Ok(());
    }
    if cfg.discord_webhook_url.is_empty() {
        return Err("DISCORD_WEBHOOK_URL 미설정".to_string());
    }
    discord::send_all(
        client,
        &cfg.discord_webhook_url,
        &messages,
        &r.images,
        &discord::real_sleep,
    )
    .await?;
    out(&format!(
        "[digest] {} 전송 완료 (판정: {}, 메시지 {}개, 이미지 {}장)",
        kind.as_str(),
        r.verdict.label,
        messages.len(),
        r.images.len()
    ));
    Ok(())
}

fn log_next(kind: Kind, target: i64) {
    // Node never sets `job.logged`, so this prints every cycle
    out(&format!(
        "[digest] {} 다음 실행: {}",
        kind.as_str(),
        iso_string(target)
    ));
}

async fn job_loop(
    client: reqwest::Client,
    cfg: Arc<Config>,
    kind: Kind,
    spec: Spec,
    mut target: i64,
) {
    loop {
        loop {
            let left = target - now_ms();
            if left <= 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(left.min(MAX_TIMER_MS) as u64)).await;
        }
        if let Err(e) = run_once(&client, &cfg, kind, false, now_ms()).await {
            safe_log(&cfg, &format!("[digest] {} 실패: {e}", kind.as_str()));
        }
        match schedule::next_run(now_ms().max(target + 1000), spec, &cfg.tz) {
            Ok(t) => {
                target = t;
                log_next(kind, target);
            }
            Err(e) => {
                // in Node this throw is an unhandled rejection, which ends the process
                safe_log(&cfg, &e);
                std::process::exit(1);
            }
        }
    }
}

async fn serve(client: reqwest::Client, cfg: Arc<Config>) -> Result<(), String> {
    let mut jobs = Vec::new();
    if cfg.daily_at.to_lowercase() != "off" {
        jobs.push((Kind::Daily, schedule::parse_spec(&cfg.daily_at, false)?));
    }
    if cfg.weekly_at.to_lowercase() != "off" {
        jobs.push((Kind::Weekly, schedule::parse_spec(&cfg.weekly_at, true)?));
    }
    if jobs.is_empty() {
        return Err("예약된 작업이 없음".to_string());
    }
    let mut handles = Vec::new();
    for (kind, spec) in jobs {
        // first target is computed up front, so a bad time zone fails startup like Node
        let target = schedule::next_run(now_ms(), spec, &cfg.tz)?;
        log_next(kind, target);
        handles.push(tokio::spawn(job_loop(
            client.clone(),
            cfg.clone(),
            kind,
            spec,
            target,
        )));
    }
    for h in handles {
        let _ = h.await;
    }
    Ok(())
}

enum Command {
    Once { kind: Kind, dry_run: bool },
    Serve,
}

/// `const [cmd, kind, ...flags] = argv`
fn parse_args(args: &[String]) -> Option<Command> {
    match args {
        [cmd, kind, flags @ ..] if cmd == "once" && (kind == "daily" || kind == "weekly") => {
            Some(Command::Once {
                kind: if kind == "weekly" {
                    Kind::Weekly
                } else {
                    Kind::Daily
                },
                dry_run: flags.iter().any(|f| f == "--dry-run"),
            })
        }
        [cmd, ..] if cmd == "serve" => Some(Command::Serve),
        _ => None,
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cfg = Arc::new(load_config_from_env());
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let Some(cmd) = parse_args(&args) else {
        let _ = writeln!(std::io::stderr(), "{USAGE}");
        return ExitCode::from(2);
    };
    let result = match http::client() {
        Err(e) => Err(e.to_string()),
        Ok(client) => match cmd {
            Command::Once { kind, dry_run } => {
                run_once(&client, &cfg, kind, dry_run, once_now()).await
            }
            Command::Serve => serve(client, cfg.clone()).await,
        },
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            safe_log(&cfg, &format!("[digest] 실패: {e}"));
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn argv_like_node() {
        assert!(matches!(
            parse_args(&args(&["once", "daily"])),
            Some(Command::Once {
                kind: Kind::Daily,
                dry_run: false
            })
        ));
        assert!(matches!(
            parse_args(&args(&["once", "weekly", "x", "--dry-run"])),
            Some(Command::Once {
                kind: Kind::Weekly,
                dry_run: true
            })
        ));
        assert!(matches!(
            parse_args(&args(&["serve", "extra"])),
            Some(Command::Serve)
        ));
        for bad in [
            &["once"][..],
            &["once", "monthly"],
            &["--dry-run", "once", "daily"],
            &[],
            &["help"],
        ] {
            assert!(parse_args(&args(bad)).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn cut_surrogate_prints_as_replacement_char() {
        assert_eq!(stdout_text("a\u{E03D}…"), "a\u{FFFD}…");
    }
}
