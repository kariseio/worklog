//! `worklog` 명령줄 진입점 (v2).
//!
//!     worklog                        # 오늘 업무일지 생성 → 설정된 곳에 저장
//!     worklog --date 2026-07-05
//!     worklog --yesterday
//!     worklog --no-llm               # LLM 요약 없이 데이터만
//!     worklog --sources git,claude
//!     worklog --dry-run              # 파일로 저장하지 않고 콘솔에 출력
//!     worklog --template retro       # 일지 템플릿 (standard | report | retro)
//!     worklog templates              # 고를 수 있는 템플릿 목록
//!     worklog note "#요청 @김팀장 결제 API 타임아웃 늘려달라"   # 메모 한 줄
//!     worklog notes [--date D]       # 그날 메모 목록
//!
//! 설정은 `~/.worklog/settings.json` 하나(v1 앱이 만든 파일 그대로 읽음), 메모는 `~/.worklog/worklog.db`.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use worklog_core::{config::Config, notes, service, summarize::Summarizer, template, time::get_tz};

#[derive(Parser, Debug)]
#[command(name = "worklog", version, about = "업무일지 생성기")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// 대상 날짜 (YYYY-MM-DD | today | yesterday)
    #[arg(long, global = true)]
    date: Option<String>,
    /// 어제로 설정
    #[arg(long)]
    yesterday: bool,
    /// 시간대 재정의 (예: Asia/Seoul)
    #[arg(long, global = true)]
    tz: Option<String>,
    /// 사용할 소스만 콤마로 (git,claude,codex,naverworks)
    #[arg(long)]
    sources: Option<String>,
    /// 일지 템플릿 (standard | report | retro). 생략하면 설정값
    #[arg(long)]
    template: Option<String>,
    /// LLM 요약 생략
    #[arg(long)]
    no_llm: bool,
    /// 파일 저장 없이 콘솔 출력
    #[arg(long)]
    dry_run: bool,
    /// 상세 로그 출력
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 메모 한 줄 저장 (#태그 · @이름 인식)
    Note {
        /// 메모 본문(여러 인자는 공백으로 이어 붙임)
        #[arg(required = true, trailing_var_arg = true)]
        text: Vec<String>,
    },
    /// 그날 메모 목록
    Notes,
    /// 고를 수 있는 일지 템플릿 목록
    Templates,
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

    match &cli.cmd {
        Some(Cmd::Note { text }) => cmd_note(&cfg, &text.join(" ")),
        Some(Cmd::Notes) => cmd_notes(&cfg, date_spec.as_deref()),
        Some(Cmd::Templates) => cmd_templates(&cfg),
        None => cmd_generate(&cfg, &cli, date_spec.as_deref()),
    }
}

fn cmd_templates(cfg: &Config) -> ExitCode {
    let current = template::resolve(&cfg.summarizer.template);
    for t in template::all() {
        let mark = if t.id == current { "*" } else { " " };
        println!("{mark} {:<9} {}  —  {}", t.id, t.name, t.description);
        // 섹션 제목 자체에 '·' 가 들어가므로(결정 · 요청 · 할 일) 구분자는 '/' 로.
        println!("    섹션: {}", t.sections.join(" / "));
    }
    println!("\n  * 는 현재 설정값. `worklog --template <id>` 로 한 번만 바꿔 쓸 수 있습니다.");
    ExitCode::SUCCESS
}

fn cmd_note(cfg: &Config, text: &str) -> ExitCode {
    let Some(store) = service::open_store() else {
        eprintln!("메모 저장소를 열 수 없습니다.");
        return ExitCode::from(1);
    };
    match notes::add_note(&store, get_tz(&cfg.timezone), text, "cli", None) {
        Ok(Some(n)) => {
            println!(
                "  ✅ 메모 저장 ({} {}) {}{}",
                n.date,
                worklog_core::time::fmt_time(Some(&n.ts), get_tz(&cfg.timezone)),
                n.text,
                notes::trailer(&notes::to_item(&n))
            );
            ExitCode::SUCCESS
        }
        Ok(None) => {
            eprintln!("빈 메모는 저장하지 않습니다.");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("메모 저장 실패: {e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_notes(cfg: &Config, date_spec: Option<&str>) -> ExitCode {
    let ctx = match service::make_context(cfg, date_spec) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let Some(store) = service::open_store() else {
        eprintln!("메모 저장소를 열 수 없습니다.");
        return ExitCode::from(1);
    };
    let items = notes::items_for(Some(&store), ctx.target_date());
    if items.is_empty() {
        println!("{} 메모 없음", ctx.target_date());
        return ExitCode::SUCCESS;
    }
    for n in &items {
        println!(
            "  [{}] {} {}{}",
            n.id,
            worklog_core::time::fmt_time(Some(&n.ts), ctx.tz()),
            n.text,
            notes::trailer(n)
        );
    }
    ExitCode::SUCCESS
}

fn cmd_generate(cfg: &Config, cli: &Cli, date_spec: Option<&str>) -> ExitCode {
    let sources: Option<Vec<String>> = cli.sources.as_ref().map(|s| vec![s.clone()]);
    let tpl = template::resolve(cli.template.as_deref().unwrap_or(&cfg.summarizer.template));
    if let Some(asked) = &cli.template
        && !asked.trim().eq_ignore_ascii_case(tpl)
    {
        tracing::warn!("알 수 없는 템플릿 {asked:?} → '{tpl}' 로 진행합니다. (worklog templates)");
    }
    tracing::info!("업무일지 생성 ({} · 템플릿 {tpl})", cfg.timezone);
    let summarizer = Summarizer::new(cfg.summarizer.clone());
    let store = service::open_store();
    let result = match service::generate_with(
        cfg,
        date_spec,
        cli.no_llm,
        sources.as_deref(),
        &summarizer,
        store.as_ref(),
        Some(tpl),
    ) {
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

    let results = service::save(cfg, worklog, None);
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
