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
//! 주간 모아보기(N8) — 이미 만들어진 일별 문서를 합쳐 **출력만** 한다(저장 없음).
//!
//!     worklog weekly                                     # 이번 주(월~오늘)
//!     worklog weekly --from 2026-09-14 --to 2026-09-18   # 기간 지정
//!     worklog --from 2026-09-14 --to 2026-09-18 --template weekly
//!     worklog weekly --out 주간.md                        # stdout 대신 파일로
//!     worklog weekly --no-llm                            # 중복 병합 LLM 호출 없이
//!
//! 설정은 `~/.worklog/settings.json` 하나(v1 앱이 만든 파일 그대로 읽음), 메모는 `~/.worklog/worklog.db`.

use std::process::ExitCode;

use chrono::{Datelike, Duration, NaiveDate};
use clap::{Parser, Subcommand};
use worklog_core::{
    config::Config, notes, service, summarize::Summarizer, template, time::get_tz,
    time::resolve_day,
};

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
    /// 일지 템플릿 (standard | report | retro). 주간은 weekly — `--from`/`--to` 와 함께
    #[arg(long)]
    template: Option<String>,
    /// LLM 요약(주간은 중복 병합) 생략
    #[arg(long, global = true)]
    no_llm: bool,
    /// 파일 저장 없이 콘솔 출력
    #[arg(long)]
    dry_run: bool,
    /// 주간 시작일 (YYYY-MM-DD)
    #[arg(long, global = true)]
    from: Option<String>,
    /// 주간 종료일 (YYYY-MM-DD)
    #[arg(long, global = true)]
    to: Option<String>,
    /// 주간 결과를 stdout 대신 이 파일로
    #[arg(long, global = true)]
    out: Option<String>,
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
    /// 주간 모아보기 — 이번 주(월~오늘) 또는 `--from`/`--to`. 저장하지 않고 출력만
    Weekly,
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
        Some(Cmd::Weekly) => cmd_weekly(&cfg, &cli),
        // `--from`/`--to` 는 기간 문서(주간)라는 뜻이다. 기간 없이 `--template weekly` 만 준 건 오류.
        None if cli.from.is_some() || cli.to.is_some() => cmd_weekly(&cfg, &cli),
        None if cli.template.as_deref().is_some_and(template::is_weekly) => {
            eprintln!(
                "`--template weekly` 에는 기간이 필요합니다. \
                 `--from 2026-09-14 --to 2026-09-18` 을 주거나 `worklog weekly` 를 쓰세요."
            );
            ExitCode::from(2)
        }
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
    println!(
        "  주간은 `worklog weekly`(= `--from … --to … --template {}`) — 이미 만든 일별 문서를 합쳐 출력만 합니다.",
        template::WEEKLY_TEMPLATE
    );
    ExitCode::SUCCESS
}

// --------------------------------------------------------------------------- //
// 주간 모아보기 (N8)
// --------------------------------------------------------------------------- //

/// 기간 — `--from`/`--to` 를 **둘 다** 주거나, 둘 다 생략하면 이번 주(월요일~오늘).
fn weekly_range(cfg: &Config, cli: &Cli) -> Result<(NaiveDate, NaiveDate), String> {
    let tz = get_tz(&cfg.timezone);
    let day = |s: &str| {
        resolve_day(Some(s), tz)
            .map(|b| b.date)
            .map_err(|e| e.to_string())
    };
    match (cli.from.as_deref(), cli.to.as_deref()) {
        (Some(f), Some(t)) => Ok((day(f)?, day(t)?)),
        (None, None) => {
            let today = resolve_day(None, tz).map_err(|e| e.to_string())?.date;
            let back = i64::from(today.weekday().num_days_from_monday());
            Ok((today - Duration::days(back), today))
        }
        _ => Err("--from 과 --to 는 함께 주세요. (예: --from 2026-09-14 --to 2026-09-18)".into()),
    }
}

/// 이미 만들어진 일별 문서를 합쳐 주간 문서 한 장. **저장하지 않는다** — stdout 또는 `--out`.
fn cmd_weekly(cfg: &Config, cli: &Cli) -> ExitCode {
    let (from, to) = match weekly_range(cfg, cli) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    if let Some(t) = &cli.template
        && !template::is_weekly(t)
    {
        tracing::warn!("기간을 주면 주간 문서만 만듭니다 — --template {t:?} 는 무시합니다.");
    }
    let Some(store) = service::open_store() else {
        eprintln!("문서 저장소를 열 수 없습니다.");
        return ExitCode::from(1);
    };
    let summarizer = (!cli.no_llm).then(|| Summarizer::new(cfg.summarizer.clone()));
    let doc = match service::generate_range(
        cfg,
        &store,
        from,
        to,
        template::WEEKLY_TEMPLATE,
        summarizer.as_ref(),
    ) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    if doc.days_included.is_empty() {
        tracing::warn!("{from} ~ {to} 구간에 일별 문서가 하나도 없습니다.");
    }
    tracing::info!(
        "일별 문서 {}일 · 빠진 평일 {}일 · 파싱 실패 {}일 · 중복 병합 LLM {}",
        doc.days_included.len(),
        doc.days_missing.len(),
        doc.parse_failures.len(),
        if doc.llm_used { "사용" } else { "미사용" }
    );

    match &cli.out {
        Some(path) => match std::fs::write(path, &doc.markdown) {
            Ok(()) => {
                println!("  ✅ 주간 업무일지: {path}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("파일로 저장하지 못했습니다({path}): {e}");
                ExitCode::from(1)
            }
        },
        None => {
            print!("{}", doc.markdown);
            ExitCode::SUCCESS
        }
    }
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
    if cli.out.is_some() {
        tracing::warn!("--out 은 주간 모아보기(`worklog weekly`) 전용입니다 — 무시합니다.");
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("파싱 성공")
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn weekly_accepts_range_flags_before_and_after_the_subcommand() {
        let c = parse(&["worklog", "weekly"]);
        assert!(matches!(c.cmd, Some(Cmd::Weekly)));
        assert!(c.from.is_none() && c.to.is_none() && c.out.is_none() && !c.no_llm);

        // 전역 플래그라 하위 명령 뒤에도 붙는다.
        let c = parse(&[
            "worklog",
            "weekly",
            "--from",
            "2026-09-14",
            "--to",
            "2026-09-18",
            "--out",
            "주간.md",
            "--no-llm",
        ]);
        assert_eq!(c.from.as_deref(), Some("2026-09-14"));
        assert_eq!(c.to.as_deref(), Some("2026-09-18"));
        assert_eq!(c.out.as_deref(), Some("주간.md"));
        assert!(c.no_llm);

        // 하위 명령 없이 `--from … --to … --template weekly`.
        let c = parse(&[
            "worklog",
            "--from",
            "2026-09-14",
            "--to",
            "2026-09-18",
            "--template",
            "weekly",
        ]);
        assert!(c.cmd.is_none());
        assert!(c.template.as_deref().is_some_and(template::is_weekly));
        // weekly 는 일별 템플릿 id 가 아니다.
        assert!(!template::TEMPLATE_IDS.contains(&template::WEEKLY_TEMPLATE));
    }

    #[test]
    fn weekly_range_defaults_to_this_week_and_needs_both_ends() {
        let cfg = Config {
            timezone: "Asia/Seoul".into(),
            ..Config::default()
        };
        let with = |args: &[&str]| weekly_range(&cfg, &parse(args));

        let (from, to) = with(&[
            "worklog",
            "weekly",
            "--from",
            "2026-09-14",
            "--to",
            "2026-09-18",
        ])
        .expect("기간 파싱");
        assert_eq!(
            (from.to_string().as_str(), to.to_string().as_str()),
            ("2026-09-14", "2026-09-18")
        );

        // 이번 주 — 월요일부터 오늘까지.
        let (from, to) = with(&["worklog", "weekly"]).expect("이번 주");
        assert_eq!(from.weekday(), chrono::Weekday::Mon);
        assert!(from <= to && (to - from).num_days() < 7);

        // 한쪽만 주면 오류.
        assert!(with(&["worklog", "weekly", "--from", "2026-09-14"]).is_err());
        assert!(with(&["worklog", "weekly", "--to", "2026-09-18"]).is_err());
        assert!(with(&["worklog", "weekly", "--from", "어제"]).is_err());
    }
}
