//! `worklog` 명령줄 진입점 (v2).
//!
//! 3단계(파이프라인·CLI)에서 v1 옵션(--date/--yesterday/--no-llm/--sources/--dry-run)과
//! `note` 하위 명령을 채운다. 지금은 뼈대만 있다.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "worklog", version, about = "업무일지 생성기")]
struct Cli {
    /// 상세 로그 출력
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let filter = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .without_time()
        .init();

    println!("worklog {}", worklog_core::version());
    Ok(())
}
