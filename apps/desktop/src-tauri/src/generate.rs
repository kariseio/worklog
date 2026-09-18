//! 일지 생성 작업 — 동시 1개, 단계별 진행 이벤트, 취소.
//!
//! 수집 → 렌더 → (LLM) 요약 → 문서 저장(SQLite) → 내보내기(markdown/obsidian/notion) 순서로
//! 별도 스레드에서 돈다. `generate:progress` / `generate:done` 이벤트로 화면에 알린다.
//! 사용자가 편집한 일지는 `overwrite_edited` 없이는 덮어쓰지 않는다(계획 §0).

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use chrono::{NaiveDate, Utc};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use worklog_core::{
    model::WorkLog,
    notes,
    output::SinkResult,
    service,
    store::{Document, run_kind, run_status},
    summarize::{SummaryOutcome, Summarizer},
    template,
};

use crate::{
    shell,
    state::{AppState, GenJob, GenStatus},
};

/// 편집된 일지가 있을 때 돌려주는 오류(화면은 이 접두어로 '덮어쓸까요?' 확인을 띄운다).
pub const EDITED_PREFIX: &str = "편집된 일지가 있습니다";

#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub run_id: i64,
    pub step: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Done {
    pub run_id: i64,
    pub date: NaiveDate,
    /// ok | failed | cancelled
    pub status: String,
    pub error: Option<String>,
    pub sinks: Vec<SinkResult>,
    pub has_summary: bool,
}

struct Outcome {
    status: &'static str,
    error: Option<String>,
    sinks: Vec<SinkResult>,
    has_summary: bool,
}

impl Outcome {
    fn failed(msg: impl Into<String>) -> Self {
        Self {
            status: run_status::FAILED,
            error: Some(msg.into()),
            sinks: Vec::new(),
            has_summary: false,
        }
    }

    fn cancelled() -> Self {
        Self {
            status: run_status::CANCELLED,
            error: None,
            sinks: Vec::new(),
            has_summary: false,
        }
    }
}

/// 생성 시작. 이미 돌고 있거나, 편집된 일지가 있는데 `overwrite_edited` 가 아니면 거절.
/// 자동 실행(`run_kind::AUTO`)은 편집된 일지를 절대 덮어쓰지 않는다. 돌려주는 값은 `runs.id`.
///
/// `template` 은 이번 한 번만 쓸 일지 템플릿 id. None 이면 설정값(`summarizer.template`)을 쓴다.
/// 모르는 id 는 [`template::resolve`] 가 기본 템플릿으로 되돌린다.
pub fn start(
    app: &AppHandle,
    date: NaiveDate,
    kind: &'static str,
    overwrite_edited: bool,
    template: Option<String>,
) -> Result<i64, String> {
    let state = app.state::<AppState>();
    let cfg_template = state.config().summarizer.template;
    let tpl = template::resolve(template.as_deref().unwrap_or(&cfg_template)).to_string();
    let mut slot = state.job();
    if let Some(j) = slot.as_ref() {
        return Err(format!(
            "이미 생성 중입니다 ({} · {})",
            j.status.date, j.status.step
        ));
    }
    let edited = state.with_store(|s| {
        Ok(s.document_get(date)
            .map_err(|e| e.to_string())?
            .is_some_and(|d| d.is_edited()))
    })?;
    if edited && (kind == run_kind::AUTO || !overwrite_edited) {
        return Err(format!(
            "{EDITED_PREFIX}({date}). 다시 만들면 편집한 내용이 사라집니다."
        ));
    }
    let run_id = state.with_store(|s| s.run_start(date, kind).map_err(|e| e.to_string()))?;
    let cancel = Arc::new(AtomicBool::new(false));
    *slot = Some(GenJob {
        status: GenStatus {
            run_id,
            date,
            kind: kind.to_string(),
            template: tpl.clone(),
            step: "준비".into(),
            detail: String::new(),
            started: Utc::now(),
        },
        cancel: cancel.clone(),
    });
    drop(slot);

    let app = app.clone();
    thread::Builder::new()
        .name(format!("worklog-gen-{run_id}"))
        .spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| run(&app, run_id, date, &cancel, &tpl)))
                .unwrap_or_else(|_| Outcome::failed("생성 중 내부 오류(패닉)"));
            finish(&app, run_id, date, outcome);
        })
        .map_err(|e| {
            // 스레드를 못 띄우면 슬롯을 비우고 실행 기록도 닫는다.
            *state.job() = None;
            let _ = state.with_store(|s| {
                s.run_finish(run_id, run_status::FAILED, Some(&e.to_string()))
                    .map_err(|e| e.to_string())
            });
            format!("생성 스레드 시작 실패: {e}")
        })?;
    Ok(run_id)
}

/// 진행 중인 생성에 취소 표시. 진행 중인 LLM 호출(claude CLI)은 바로 죽는다. 없으면 false.
pub fn cancel(app: &AppHandle) -> bool {
    let state = app.state::<AppState>();
    let mut slot = state.job();
    let Some(j) = slot.as_mut() else {
        return false;
    };
    j.cancel.store(true, Ordering::Relaxed);
    j.status.step = "취소 중".into();
    j.status.detail.clear();
    let run_id = j.status.run_id;
    drop(slot);
    let _ = app.emit(
        "generate:progress",
        Progress {
            run_id,
            step: "취소 중".into(),
            detail: String::new(),
        },
    );
    true
}

fn run(
    app: &AppHandle,
    run_id: i64,
    date: NaiveDate,
    cancel: &Arc<AtomicBool>,
    tpl: &str,
) -> Outcome {
    let state = app.state::<AppState>();
    let cfg = state.config();
    let progress: Arc<worklog_core::summarize::ProgressFn> = {
        let app = app.clone();
        let cancel = cancel.clone();
        Arc::new(move |step: &str, detail: &str| {
            if cancel.load(Ordering::Relaxed) {
                return; // '취소 중' 표시를 덮어쓰지 않는다.
            }
            let state = app.state::<AppState>();
            if let Some(j) = state.job().as_mut()
                && j.status.run_id == run_id
            {
                j.status.step = step.to_string();
                j.status.detail = detail.to_string();
            }
            let _ = app.emit(
                "generate:progress",
                Progress {
                    run_id,
                    step: step.to_string(),
                    detail: detail.to_string(),
                },
            );
        })
    };
    let is_cancelled = || cancel.load(Ordering::Relaxed);

    progress("수집", "");
    let ctx = match service::make_context(&cfg, Some(&date.to_string())) {
        Ok(c) => c,
        Err(e) => return Outcome::failed(e.to_string()),
    };
    let note_items = state
        .with_store(|s| Ok(notes::items_for(Some(s), date)))
        .unwrap_or_default();
    let wanted = service::enabled_sources(&cfg, None);
    let service::Collected { data, statuses } = service::collect(&cfg, &ctx, &wanted, note_items);
    if is_cancelled() {
        return Outcome::cancelled();
    }
    let rendered = service::render_all(&cfg, &data, &statuses, ctx.tz());

    let outcome = if data.is_empty() {
        progress("요약", "수집된 활동이 없어 건너뜀");
        SummaryOutcome::default()
    } else {
        progress("요약", "");
        Summarizer::new(cfg.summarizer.clone())
            .with_progress(progress.clone())
            .with_cancel(cancel.clone())
            .summarize_day_outcome(
                &template::system_prompt(tpl),
                &rendered.signal,
                &date.to_string(),
                &rendered.availability,
            )
    };
    let (summary, summary_error) = (outcome.text, outcome.error);
    if is_cancelled() {
        return Outcome::cancelled();
    }

    progress("저장", "");
    let full = template::compose(
        tpl,
        date,
        summary.as_deref(),
        summary_error.as_deref(),
        &rendered.analysis,
        ctx.tz(),
        &rendered.facts,
        cfg.include_raw_data,
    );
    let worklog = WorkLog {
        target_date: date,
        facts_markdown: rendered.facts.clone(),
        full_markdown: full.clone(),
        data,
        summary_markdown: summary.clone(),
    };
    let doc = Document {
        date,
        summary_md: summary.clone(),
        full_md: full,
        generated_at: Utc::now(),
        edited_at: None,
        run_id: Some(run_id),
        template: Some(tpl.to_string()),
    };
    if let Err(e) = state.with_store(|s| s.document_put(&doc).map_err(|e| e.to_string())) {
        return Outcome::failed(format!("문서 저장 실패: {e}"));
    }
    let sinks = service::save(&cfg, &worklog, None);
    let failed: Vec<String> = sinks
        .iter()
        .filter(|s| !s.ok)
        .map(|s| format!("{}: {}", s.name, s.error.clone().unwrap_or_default()))
        .collect();
    // 저장은 됐지만 반쪽인 경우 — AI 요약 실패와 내보내기 실패를 Done.error 한 줄로(N6 토스트가 읽는다).
    let mut notes: Vec<String> = Vec::new();
    if let Some(e) = &summary_error {
        notes.push(format!("AI 요약 실패: {e}"));
    }
    if !failed.is_empty() {
        notes.push(format!("일부 내보내기 실패 — {}", failed.join(" / ")));
    }
    Outcome {
        status: run_status::OK,
        error: (!notes.is_empty()).then(|| notes.join(" / ")),
        sinks,
        has_summary: summary.is_some(),
    }
}

fn finish(app: &AppHandle, run_id: i64, date: NaiveDate, outcome: Outcome) {
    let state = app.state::<AppState>();
    if let Err(e) = state.with_store(|s| {
        s.run_finish(run_id, outcome.status, outcome.error.as_deref())
            .map_err(|e| e.to_string())
    }) {
        tracing::warn!("실행 기록 마감 실패(run {run_id}): {e}");
    }
    *state.job() = None;
    let _ = app.emit(
        "generate:done",
        Done {
            run_id,
            date,
            status: outcome.status.to_string(),
            error: outcome.error.clone(),
            sinks: outcome.sinks.clone(),
            has_summary: outcome.has_summary,
        },
    );
    match outcome.status {
        run_status::OK => {
            let body = match &outcome.error {
                Some(e) => format!("{date} 일지를 저장했습니다({e})."),
                None if outcome.has_summary => format!("{date} 일지를 저장했습니다."),
                None => format!("{date} 일지를 저장했습니다(AI 요약 없음)."),
            };
            shell::notify(app, "생성 완료", &body);
        }
        run_status::FAILED => shell::notify(
            app,
            "생성 실패",
            outcome.error.as_deref().unwrap_or("알 수 없는 오류"),
        ),
        _ => {}
    }
}
