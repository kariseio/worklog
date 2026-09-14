//! `worklog` 명령줄 진입점 (v2).
//!
//!     worklog                     # 오늘 업무일지 생성 → 설정된 곳에 저장
//!     worklog --date 2026-07-05
//!     worklog --yesterday
//!     worklog --no-llm            # LLM 요약 없이 데이터만
//!     worklog --sources git,claude
//!     worklog --dry-run           # 파일로 저장하지 않고 콘솔에 출력
//!
//! 설정은 `~/.worklog/settings.json` 하나(v1 앱이 만든 파일 그대로 읽음).

use std::process::ExitCode;

use clap::Parser;
use worklog_core::{config::Config, service};

#[derive(Parser, Debug)]
#[command(name = "worklog", version, about = "업무일지 생성기")]
struct Cli {
    /// 대상 날짜 (YYYY-MM-DD | today | yesterday)
    #[arg(long)]
    date: Option<String>,
    /// 어제로 설정
    #[arg(long)]
    yesterday: bool,
    /// 시간대 재정의 (예: Asia/Seoul)
    #[arg(long)]
    tz: Option<String>,
    /// 사용할 소스만 콤마로 (git,claude,codex,naverworks)
    #[arg(long)]
    sources: Option<String>,
    /// LLM 요약 생략
    #[arg(long)]
    no_llm: bool,
    /// 파일 저장 없이 콘솔 출력
    #[arg(long)]
    dry_run: bool,
    /// 상세 로그 출력
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let filter = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_writer(std::io::stderr)
        .without_time()
        .init();

    let mut cfg = Config::load();
    if let Some(tz) = &cli.tz {
        cfg.timezone = tz.clone();
    }
    let date_spec = if cli.yesterday {
        Some("yesterday".to_string())
    } else {
        cli.date.clone()
    };
    let sources: Option<Vec<String>> = cli.sources.as_ref().map(|s| vec![s.clone()]);

    tracing::info!("업무일지 생성 ({})", cfg.timezone);
    let result = match service::generate(&cfg, date_spec.as_deref(), cli.no_llm, sources.as_deref())
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::from(2);
        }
    };
    let worklog = &result.worklog;
    if worklog.data.is_empty() {
        tracing::info!("수집된 데이터가 없습니다.");
    }

    if cli.dry_run {
        print!("{}", worklog.full_markdown);
        return ExitCode::SUCCESS;
    }

    let results = service::save(&cfg, worklog, None);
    if results.is_empty() {
        tracing::warn!("활성화된 출력이 없습니다. 설정의 저장 대상을 확인하세요.");
        return ExitCode::SUCCESS;
    }
    println!();
    for r in &results {
        if r.ok {
            println!("  ✅ {}: {}", r.name, r.location.as_deref().unwrap_or(""));
        } else {
            println!("  ❌ {}: {}", r.name, r.error.as_deref().unwrap_or(""));
        }
    }
    ExitCode::SUCCESS
}
