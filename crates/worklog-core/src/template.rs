//! 일지 템플릿 — 같은 하루 데이터를 '표준 · 보고용 · 회고용' 세 가지 문서로 낸다.
//!
//! 템플릿 하나가 두 가지를 정한다.
//!   1) [`system_prompt`] — LLM 에게 줄 출력 구조(섹션 제목·지시문·분량).
//!   2) [`compose`] — LLM 결과 위아래에 붙는 결정론적 부분(제목 줄 · 지표 · 타임라인 · 원본 부록).
//!
//! 섹션 제목은 데이터로만 정의한다 — 프롬프트에 쓰는 제목과 문서에 남는 제목이 어긋나지 않게.

use chrono::{Datelike, NaiveDate};
use chrono_tz::Tz;
use serde::Serialize;

use crate::{
    analyze::Analysis,
    render::{render_focus_table, render_metrics_line, render_timeline_list},
    weekly::{NONE_LINE, RangeKpis, find_metrics, fmt_md},
};

pub const DEFAULT_TEMPLATE: &str = "standard";
pub const TEMPLATE_IDS: [&str; 3] = ["standard", "report", "retro"];

/// 주간 모아보기(N8) 전용 id. **[`TEMPLATE_IDS`] 에 넣지 않는다** — 하루 문서 템플릿이 아니고,
/// `documents` 에 저장되지도 않는다(`worklog weekly` 의 출력 전용).
pub const WEEKLY_TEMPLATE: &str = "weekly";

/// `--template weekly` 인지. 앞뒤 공백·대소문자는 눈감아 준다.
pub fn is_weekly(id: &str) -> bool {
    id.trim().eq_ignore_ascii_case(WEEKLY_TEMPLATE)
}

// --------------------------------------------------------------------------- //
// 섹션 제목 · 지시문
// --------------------------------------------------------------------------- //

/// 모든 템플릿 공통 — 주간 모아보기([`crate::weekly`])가 이 둘만 모은다.
pub const T_WINS: &str = "오늘의 성과";
/// 모든 템플릿 공통 — 주간 모아보기([`crate::weekly`])가 이 둘만 모은다.
pub const T_DECISIONS: &str = "결정 · 요청 · 할 일";
const T_PROJECTS: &str = "프로젝트별 진행";
const T_FLOW: &str = "시간대별 흐름";
const T_LEARNED: &str = "막힌 것 · 배운 것";

/// 결정론적 섹션 제목(LLM 이 쓰지 않는다).
const T_METRICS: &str = "지표";
const T_TIMELINE: &str = "타임라인";

const I_WINS: &str = concat!(
    "3~6줄. 그날 실제로 끝낸 결과만. 프로젝트를 알 수 있으면 각 줄을 [프로젝트] 태그로 시작하고, ",
    "주간보고에 그대로 옮겨 붙일 수 있는 문장으로 쓴다. ",
    "규칙 7 대로 줄 끝에 근거 괄호(`(a1b2c3d)` · `(auth.py)` · `(회의: …)` · `(메모 10:35)`)를 반드시 붙이고, ",
    "근거가 없으면 그 줄은 빼서 3줄 미만이 돼도 좋다. 커밋 0건인 날은 규칙 8 대로 완료 단정을 쓰지 않는다."
);
const I_DECISIONS: &str = concat!(
    "결정은 \"결정: …\", 구두 요청은 \"요청(@이름): …\", 앞으로 할 일은 체크박스 \"- [ ] …\" 로 쓴다. ",
    "'메모'(사용자가 직접 남긴 1차 사실)는 빠짐없이 반영하고, 메모에서 온 줄에는 규칙 9 의 ",
    "`[메모 · 요청]`/`[메모 · 결정]`/`[메모 · 할일]` 표기를 줄 끝에 붙인다. 세션·커밋에서 드러난 결정도 포함한다. ",
    "해당하는 것이 하나도 없으면 \"- (없음)\" 한 줄만."
);
const I_PROJECTS: &str = concat!(
    "프로젝트마다 **굵은 이름** 아래 2~4줄. 각 줄은 `내용 (근거) (상태)` 형식 — 규칙 7 의 근거 괄호를 먼저 붙이고 ",
    "그 뒤에 상태를 (완료) · (진행 중) · (막힘) 중 하나로 붙인다. 근거를 댈 수 없는 줄은 쓰지 않는다. ",
    "커밋 0건인 날은 (완료) 를 쓰지 않는다(규칙 8). ",
    "한 프로젝트를 여러 세션에서 다뤘으면 합쳐서 정리한다."
);
const I_FLOW_BRIEF: &str = concat!(
    "**굵은 시간대**(예: **09–12시**) 블록으로 묶고 블록당 2~4줄로 압축한다. ",
    "회의는 반드시 해당 시각에 명시한다. 메모에서 온 줄에는 규칙 9 의 `[메모 · …]` 표기를 붙인다."
);
const I_FLOW_FULL: &str = concat!(
    "**굵은 시간대** 블록으로 묶어 시각과 근거(어느 세션·어느 커밋인지)까지 자세히 쓴다. ",
    "회의는 반드시 해당 시각에 명시한다. 메모에서 온 줄에는 규칙 9 의 `[메모 · …]` 표기를 붙인다."
);
const I_LEARNED: &str = "2~5줄. 막혔던 지점과 거기서 알게 된 것을 한 줄씩.";

// --------------------------------------------------------------------------- //
// 템플릿 정의
// --------------------------------------------------------------------------- //

/// LLM 이 쓸 섹션 하나 — 문서에 남을 제목과 그 섹션의 지시문.
struct SectionSpec {
    title: &'static str,
    instruction: &'static str,
}

/// 결정론적(사실) 섹션의 상세 정도.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Metrics {
    /// 지표 한 줄 + 프로젝트별 집중 표 + 접어 둔 타임라인.
    Full,
    /// 지표 한 줄만.
    LineOnly,
}

struct Template {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    /// LLM 이 이 순서대로 쓴다.
    sections: &'static [SectionSpec],
    metrics: Metrics,
}

const STANDARD: Template = Template {
    id: "standard",
    name: "표준",
    description: "성과 · 결정 · 프로젝트 · 시간대별 흐름을 모두 담은 기본 일지",
    sections: &[
        SectionSpec {
            title: T_WINS,
            instruction: I_WINS,
        },
        SectionSpec {
            title: T_DECISIONS,
            instruction: I_DECISIONS,
        },
        SectionSpec {
            title: T_PROJECTS,
            instruction: I_PROJECTS,
        },
        SectionSpec {
            title: T_FLOW,
            instruction: I_FLOW_BRIEF,
        },
    ],
    metrics: Metrics::Full,
};

const REPORT: Template = Template {
    id: "report",
    name: "보고용",
    description: "성과와 결정 · 요청만 추린 짧은 보고용 일지",
    sections: &[
        SectionSpec {
            title: T_WINS,
            instruction: I_WINS,
        },
        SectionSpec {
            title: T_DECISIONS,
            instruction: I_DECISIONS,
        },
    ],
    metrics: Metrics::LineOnly,
};

const RETRO: Template = Template {
    id: "retro",
    name: "회고용",
    description: "시간대별 흐름과 막힌 것 · 배운 것까지 자세히 남기는 회고용 일지",
    sections: &[
        SectionSpec {
            title: T_FLOW,
            instruction: I_FLOW_FULL,
        },
        SectionSpec {
            title: T_PROJECTS,
            instruction: I_PROJECTS,
        },
        SectionSpec {
            title: T_LEARNED,
            instruction: I_LEARNED,
        },
        SectionSpec {
            title: T_DECISIONS,
            instruction: I_DECISIONS,
        },
    ],
    metrics: Metrics::Full,
};

/// [`TEMPLATE_IDS`] 와 같은 순서.
const TEMPLATES: [&Template; 3] = [&STANDARD, &REPORT, &RETRO];

// --------------------------------------------------------------------------- //
// 공개 API
// --------------------------------------------------------------------------- //

/// 화면·CLI 에 뿌릴 템플릿 정보.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TemplateInfo {
    pub id: &'static str,
    /// 표준 · 보고용 · 회고용
    pub name: &'static str,
    pub description: &'static str,
    /// 문서에 나오는 섹션 제목(순서대로 — LLM 섹션 + 결정론적 섹션).
    pub sections: Vec<&'static str>,
}

pub fn all() -> Vec<TemplateInfo> {
    TEMPLATES.iter().map(|t| info(t)).collect()
}

/// 아는 id 면 그 id, 모르면 [`DEFAULT_TEMPLATE`]. 앞뒤 공백·대소문자는 눈감아 준다.
pub fn resolve(id: &str) -> &'static str {
    let want = id.trim().to_ascii_lowercase();
    TEMPLATE_IDS
        .into_iter()
        .find(|t| *t == want)
        .unwrap_or(DEFAULT_TEMPLATE)
}

/// 그 템플릿으로 일지를 쓸 때 LLM 에 줄 system 프롬프트(한국어 전문).
///
/// 첫 문장은 [`crate::render::is_meta_session`] 의 판별 서명과 일치해야 한다 — 이 호출이 만드는
/// Claude 세션이 다음 날 수집에 섞여 들어가지 않게.
pub fn system_prompt(id: &str) -> String {
    let t = get(id);
    let mut s = String::with_capacity(2048);
    s.push_str(INTRO);
    s.push_str(RULES);
    s.push_str("출력 구조:\n");
    s.push_str(
        "- 맨 앞: 인용문 한 줄 `> …` — 오늘을 한 문장으로. 제목(#)으로 시작하지 마라(문서 제목은 이미 있다).\n",
    );
    for sec in t.sections {
        s.push_str("- `## ");
        s.push_str(sec.title);
        s.push_str("` — ");
        s.push_str(sec.instruction);
        s.push('\n');
    }
    s.push_str("\n전체가 한눈에 들어오게. 문단 쓰지 말고 불릿만 써라.");
    s
}

/// 최종 문서: 제목 줄 + LLM 본문 + (템플릿별) 지표·타임라인 + (선택) 수집 원본 부록.
///
/// `summary_error` 는 요약 **시도가 실패한 사유**([`crate::summarize::SummaryOutcome::error`]).
/// 이게 있으면 문서가 '요약을 안 한 날' 과 '요약이 실패한 날' 을 구분해 적는다 —
/// 요약이 통째로 없으면 경고 한 줄, 부분 요약이면 본문 아래 한 줄.
// 문서 한 장을 조립하는 데 필요한 값이 그대로 인자다 — 묶어서 구조체로 만들면
// 호출부(서비스·앱·테스트)가 오히려 읽기 어려워진다.
#[allow(clippy::too_many_arguments)]
pub fn compose(
    id: &str,
    target: NaiveDate,
    summary: Option<&str>,
    summary_error: Option<&str>,
    analysis: &Analysis,
    tz: Tz,
    facts: &str,
    include_raw: bool,
) -> String {
    let t = get(id);
    let mut lines: Vec<String> = vec![
        format!("# 업무일지 {target} ({})", weekday_ko(target)),
        String::new(),
    ];
    let err = summary_error.map(str::trim).filter(|e| !e.is_empty());
    match (summary.map(str::trim).filter(|s| !s.is_empty()), err) {
        // 부분 요약 — 본문은 살리되 무엇이 빠졌는지 바로 아래 한 줄로.
        (Some(s), Some(e)) => {
            lines.push(s.to_string());
            lines.push(String::new());
            lines.push(format!("> ⚠ 요약 일부만 생성됨: {e}"));
        }
        (Some(s), None) => lines.push(s.to_string()),
        // 시도했다가 실패 — '사용하지 않았습니다' 로 뭉뚱그리지 않는다(V2).
        (None, Some(e)) => lines.push(format!(
            "> ⚠ AI 요약 시도가 실패했습니다: {e}. 아래는 수집 데이터만 정리한 것입니다."
        )),
        (None, None) => {
            lines.push("> LLM 요약을 사용하지 않았습니다. 아래 지표를 참고하세요.".into())
        }
    }

    lines.push(String::new());
    lines.push(format!("## {T_METRICS}"));
    lines.push(render_metrics_line(analysis, tz));
    if t.metrics == Metrics::Full {
        let table = render_focus_table(analysis);
        if !table.is_empty() {
            lines.push(String::new());
            lines.push("**프로젝트별 집중**".into());
            lines.push(String::new());
            lines.push(table);
        }
        let timeline = render_timeline_list(analysis, tz);
        if !timeline.is_empty() {
            lines.push(String::new());
            lines.push("<details>".into());
            lines.push(format!("<summary>{T_TIMELINE}</summary>"));
            lines.push(String::new());
            lines.push(timeline);
            lines.push(String::new());
            lines.push("</details>".into());
        }
    }

    if include_raw {
        lines.extend(
            [
                "",
                "---",
                "",
                "<details>",
                "<summary>수집 데이터 원본</summary>",
                "",
                facts.trim(),
                "",
                "</details>",
            ]
            .iter()
            .map(|s| (*s).to_string()),
        );
    }
    format!("{}\n", lines.join("\n").trim_end())
}

// --------------------------------------------------------------------------- //
// 주간 모아보기 (N8)
// --------------------------------------------------------------------------- //

/// 주간 문서의 섹션 제목.
const T_W_WINS: &str = "이번 주 성과";
const T_W_DECISIONS: &str = "결정 · 요청";
const T_W_TODO: &str = "남은 할 일";
const T_W_METRICS: &str = "지표 (주간 합계)";

/// 주간 중복 병합 LLM 호출의 system 프롬프트.
///
/// 이 호출이 하는 일은 **중복 줄 합치기 하나뿐**이다 — 새 사실을 쓰게 하지 않는다.
/// (이 호출이 만드는 claude 세션은 [`crate::render::WORKLOG_SENTINEL`] 로 걸러진다.)
pub fn weekly_system_prompt() -> String {
    WEEKLY_SYSTEM.to_string()
}

const WEEKLY_SYSTEM: &str = concat!(
    "너는 여러 날의 업무일지에서 뽑아 놓은 '이번 주 성과' 초안을 다듬는 도구다. ",
    "하는 일은 단 하나 — 같은 일을 가리키는 중복 줄을 하나로 합치는 것.\n\n",
    "규칙:\n",
    "1. 새로운 사실을 만들지 마라. 입력에 없는 내용·숫자·이름·프로젝트를 추가하지 마라. ",
    "합치는 것 말고는 아무것도 하지 않는다.\n",
    "2. 날짜 앵커 `(9/16)` 와 근거 괄호 `(a1b2c3d)`·`(auth.py)` 를 지우지 마라. ",
    "여러 날을 합쳤으면 날짜를 모두 남긴다 — `… (9/15, 9/16)`.\n",
    "3. `**프로젝트**` 굵은 줄과 그 아래 불릿 구조를 그대로 유지한다. ",
    "프로젝트를 새로 만들거나 줄을 다른 프로젝트로 옮기지 마라.\n",
    "4. 서로 다른 일이면 합치지 마라. 애매하면 그대로 둔다 — 줄을 잃는 것보다 중복이 낫다.\n",
    "5. 한 프로젝트 안에서는 최신 날짜가 위로.\n",
    "6. 제목(#)·머리말·맺음말·설명을 쓰지 마라. 본문(굵은 프로젝트 줄 + 불릿)만 출력한다."
);

/// [`compose_weekly`] 입력 — 결정론적으로 합쳐 둔 조각들([`crate::weekly::merge`] 결과).
#[derive(Debug, Clone, Copy)]
pub struct WeeklyParts<'a> {
    pub from: NaiveDate,
    pub to: NaiveDate,
    /// `**프로젝트**` + 불릿. LLM 병합본이거나 [`crate::weekly::render_wins`] 결과.
    pub wins_md: &'a str,
    /// `9/16 결정: …`(앞에 `- ` 를 붙여 낸다).
    pub decisions: &'a [String],
    /// `- [ ] … (9/16 ~)` — 이미 불릿 표기가 붙어 있다.
    pub remaining: &'a [String],
    /// 체크됐거나 뒷날 완료된 할 일 수.
    pub done: usize,
    pub kpis: &'a RangeKpis,
    /// 문서가 없어 빠진 평일.
    pub missing: &'a [NaiveDate],
    /// 섹션을 찾지 못해 원문을 붙일 날 — (날짜, 원문). 원문에 지표 줄이 살아 있으면
    /// `kpis` 에 이미 합산돼 있고, 경고 문구도 그렇게 적는다([`compose_weekly`]).
    pub parse_failures: &'a [(NaiveDate, String)],
    /// 사용자가 편집한 날(편집본이 정본).
    pub edited: &'a [NaiveDate],
    /// LLM 병합을 실제로 썼는가.
    pub llm_used: bool,
}

/// 주간 문서 한 장. 저장하지 않는다 — stdout 또는 `--out` 파일로만 나간다.
pub fn compose_weekly(p: &WeeklyParts<'_>) -> String {
    let to_short = if p.from.year() == p.to.year() {
        p.to.format("%m-%d").to_string()
    } else {
        p.to.to_string()
    };
    let mut lines: Vec<String> = vec![
        format!("# 주간 업무일지 {} ~ {to_short}", p.from),
        String::new(),
    ];

    lines.push(format!("## {T_W_WINS}"));
    let wins = p.wins_md.trim();
    lines.push(if wins.is_empty() {
        NONE_LINE.to_string()
    } else {
        wins.to_string()
    });
    lines.push(String::new());

    lines.push(format!("## {T_W_DECISIONS}"));
    if p.decisions.is_empty() {
        lines.push(NONE_LINE.to_string());
    } else {
        lines.extend(p.decisions.iter().map(|d| format!("- {d}")));
    }
    lines.push(String::new());

    lines.push(format!("## {T_W_TODO} ({})", p.remaining.len()));
    if p.remaining.is_empty() {
        lines.push(NONE_LINE.to_string());
    } else {
        lines.extend(p.remaining.iter().cloned());
    }
    if p.done > 0 {
        lines.push(String::new());
        lines.push(format!("_이번 주 완료 {}건_", p.done));
    }
    lines.push(String::new());

    lines.push(format!("## {T_W_METRICS}"));
    lines.push(p.kpis.line());

    // 빠진 날·파싱 실패는 침묵하지 않는다(원칙 5).
    let mut warnings: Vec<String> = p
        .missing
        .iter()
        .map(|d| {
            format!(
                "⚠ {} ({}) 일지가 없어 빠졌습니다",
                fmt_md(*d),
                weekday_ko(*d)
            )
        })
        .collect();
    warnings.extend(p.parse_failures.iter().map(|(d, raw)| {
        // 섹션이 깨졌어도 지표 줄은 살아 있을 수 있다 — 합산했는지를 그대로 말한다(원칙 5).
        let metrics = if find_metrics(raw).is_some() {
            "지표는 합산"
        } else {
            "지표 합계에서도 빠짐"
        };
        format!(
            "⚠ {} — 섹션을 찾지 못해 원문 그대로 포함({metrics})",
            fmt_md(*d)
        )
    }));
    if !warnings.is_empty() {
        lines.push(String::new());
        lines.extend(warnings);
    }

    let mut notes: Vec<String> = Vec::new();
    if !p.edited.is_empty() {
        let days: Vec<String> = p.edited.iter().map(|d| fmt_md(*d)).collect();
        notes.push(format!("_({} 편집본 기준)_", days.join(" · ")));
    }
    if !p.llm_used {
        notes.push("_중복 병합(LLM)을 쓰지 않아 날짜별 줄을 그대로 합쳤습니다._".to_string());
    }
    if !notes.is_empty() {
        lines.push(String::new());
        lines.extend(notes);
    }

    for (d, raw) in p.parse_failures {
        lines.push(String::new());
        lines.push(format!("### {} 원문", fmt_md(*d)));
        lines.push(String::new());
        lines.push(raw.trim().to_string());
    }

    format!("{}\n", lines.join("\n").trim_end())
}

// --------------------------------------------------------------------------- //
// 내부
// --------------------------------------------------------------------------- //

const INTRO: &str = concat!(
    "너는 하루치 개발 활동 로그(Claude Code 세션 질답 + git 커밋 + 일정 + 메모)를 '업무일지'로 ",
    "문서화하는 도구다. 목표 — '무엇을 완료했고 무엇이 결정·요청됐는가'를 그날 흐름과 함께 ",
    "한눈에 보이게 정리하는 것.\n\n",
);

const RULES: &str = concat!(
    "규칙:\n",
    "1. 세션의 '질답 흐름'을 근거로 그날 다룬 주제·요청·결정·완료를 문서화한다. ",
    "한 세션에서 여러 주제를 다뤘으면 그 주제들을 **모두** 반영한다(첫 주제만 쓰지 말 것).\n",
    "2. 각 항목은 한 줄, 개조식(명사구·완료형). 장황체·미사여구·불필요한 이모지 금지.\n",
    "3. 완료·결정된 것을 앞세우되, 중요한 '방향 전환·결정'도 한 줄로 남긴다. ",
    "단 '~하려고 했다' 식 공허한 과정 나열은 피하고 결과·결정 중심으로.\n",
    "4. git 커밋을 완료 결과의 1차 근거로 삼는다. 원본 프롬프트·명령어·파일 목록을 그대로 나열하지 마라.\n",
    "5. 데이터에 없는 것은 지어내지 마라. 제공되지 않은 소스의 내용은 만들지 마라.\n",
    "6. '메모' 는 사용자가 직접 남긴 1차 사실(구두 요청·결정·할 일)이다. 반드시 반영하고 ",
    "요청자(@이름)를 그대로 명시한다.\n",
    "7. 근거 앵커 — 성과·진행을 말하는 줄은 끝에 근거 하나를 괄호로 붙인다. ",
    "근거는 **이 프롬프트에 들어온 오늘 데이터 안에 있는 것만** 쓴다. ",
    "커밋 해시 7자리 `(a1b2c3d)` 는 아래 '## Git 커밋' 목록(오늘 커밋)에 실제로 적힌 해시만 그대로 옮긴다 — ",
    "기억·추측으로 해시를 지어내지 말고, 다른 날·다른 저장소의 해시를 끌어오지 마라. ",
    "오늘 커밋 목록이 없거나 그 줄에 맞는 해시가 없으면 해시 대신 편집 파일명 `(auth.py)` · ",
    "회의 제목 `(회의: 코드리뷰)` · 메모 시각 `(메모 10:35)` 중 **오늘 데이터에 있는 것** 하나를 쓴다. ",
    "데이터에서 근거를 찾을 수 없는 줄은 아예 쓰지 마라 — ",
    "줄 수를 채우려고 근거 없는 줄을 만들지 마라.\n",
    "8. 커밋 0건인 날 — 신호에 '## Git 커밋' 섹션이 없거나 머리글의 가용 데이터 줄에 `Git ✅(N)` 의 N 이 ",
    "보이지 않으면 그날 커밋은 0건이다. 이때는 '완료'·'마무리'·'확정' 같은 완료 단정을 쓰지 말고 ",
    "'진행'·'검토'·'논의'·'초안' 으로 서술한다(커밋이 있는 날에만 완료로 단정한다). ",
    "진행 범위·단계 숫자('N단계 완료'·'전부 이식'·'모두 마침' 같은 범위 단정)는 그날 데이터의 ",
    "커밋 메시지나 사용자 발화가 **같은 범위를 말할 때만** 쓴다. 근거가 그보다 좁으면 ",
    "근거에 적힌 범위 그대로 적는다 — 커밋이 0~5단계만 덮으면 '0~8단계 이식 완료' 가 아니라 ",
    "'4단계 완료 · 5단계 진행 중' 으로. 남은 단계를 완료 쪽으로 올려 묶지 마라. ",
    "단계·순번 번호('N단계', '#N', 'Phase N')는 그날 데이터의 커밋 메시지나 사용자 발화에 ",
    "**그 번호가 실제로 보일 때만** 쓴다 — 보이지 않으면 번호 없이 작업 내용만 적고, ",
    "번호를 추측해 붙이지 마라.\n",
    "9. 메모에서 나온 줄은 끝에 `[메모 · 요청]` / `[메모 · 결정]` / `[메모 · 할일]` 을 붙인다. ",
    "메모 줄의 `#요청`·`#결정`·`#할일` 태그를 그대로 쓰고, 태그가 없으면 `[메모]` 만 붙인다. ",
    "근거 괄호와 같이 쓸 때는 `… (메모 10:35) [메모 · 요청]` 순서로.\n\n",
);

fn get(id: &str) -> &'static Template {
    let id = resolve(id);
    TEMPLATES
        .into_iter()
        .find(|t| t.id == id)
        .unwrap_or(&STANDARD)
}

fn info(t: &'static Template) -> TemplateInfo {
    let mut sections: Vec<&'static str> = t.sections.iter().map(|s| s.title).collect();
    sections.push(T_METRICS);
    if t.metrics == Metrics::Full {
        sections.push(T_TIMELINE);
    }
    TemplateInfo {
        id: t.id,
        name: t.name,
        description: t.description,
        sections,
    }
}

/// 월화수목금토일.
fn weekday_ko(d: NaiveDate) -> &'static str {
    const KO: [&str; 7] = ["월", "화", "수", "목", "금", "토", "일"];
    KO[d.weekday().num_days_from_monday() as usize]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{analyze::analyze, time::get_tz};

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 28).unwrap()
    }

    fn sample_analysis() -> Analysis {
        analyze(&crate::analyze::tests::sample(), get_tz("Asia/Seoul"))
    }

    // ---- 주간 모아보기(N8) ------------------------------------------------ //

    fn ymd(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn weekly_id_is_not_a_daily_template() {
        assert!(!TEMPLATE_IDS.contains(&WEEKLY_TEMPLATE));
        assert!(all().iter().all(|t| t.id != WEEKLY_TEMPLATE));
        // 일별 템플릿으로는 여전히 모르는 id — 표준으로 떨어진다(TEMPLATE_IDS 불변).
        assert_eq!(resolve(WEEKLY_TEMPLATE), DEFAULT_TEMPLATE);
        assert!(is_weekly(" Weekly ") && is_weekly("weekly"));
        assert!(!is_weekly("report") && !is_weekly(""));
    }

    #[test]
    fn weekly_prompt_only_merges_and_forbids_new_facts() {
        let p = weekly_system_prompt();
        assert!(p.starts_with("너는 여러 날의 업무일지"));
        assert!(p.contains("같은 일을 가리키는 중복 줄을 하나로 합치는 것"));
        assert!(p.contains("새로운 사실을 만들지 마라"));
        assert!(p.contains("`(9/16)`")); // 날짜 앵커 보존
        assert!(p.contains("`**프로젝트**` 굵은 줄")); // 프로젝트 묶음 보존
        assert!(p.contains("애매하면 그대로 둔다"));
        assert!(p.contains("제목(#)·머리말·맺음말·설명을 쓰지 마라"));
        // 하루 템플릿 프롬프트와 섞이지 않는다.
        for id in TEMPLATE_IDS {
            assert_ne!(p, system_prompt(id));
        }
    }

    #[test]
    fn compose_weekly_has_wins_decisions_todo_metrics_then_warnings() {
        let k = RangeKpis {
            commits: 24,
            repos: 6,
            sessions: 68,
            meetings: 9,
        };
        let md = compose_weekly(&WeeklyParts {
            from: ymd(2026, 9, 14),
            to: ymd(2026, 9, 18),
            wins_md: "**alpha**\n- MCP 인증 방향 확정 (a1b2c3d) (9/16)",
            decisions: &["9/16 결정: 색인 상태 기준으로 전환".to_string()],
            remaining: &["- [ ] 재시도 루프 수정 (9/16 ~)".to_string()],
            done: 2,
            kpis: &k,
            missing: &[ymd(2026, 9, 17)],
            parse_failures: &[(ymd(2026, 9, 16), "종일 회의.".to_string())],
            edited: &[ymd(2026, 9, 16)],
            llm_used: true,
        });
        assert_eq!(
            md,
            "# 주간 업무일지 2026-09-14 ~ 09-18\n\
             \n\
             ## 이번 주 성과\n\
             **alpha**\n\
             - MCP 인증 방향 확정 (a1b2c3d) (9/16)\n\
             \n\
             ## 결정 · 요청\n\
             - 9/16 결정: 색인 상태 기준으로 전환\n\
             \n\
             ## 남은 할 일 (1)\n\
             - [ ] 재시도 루프 수정 (9/16 ~)\n\
             \n\
             _이번 주 완료 2건_\n\
             \n\
             ## 지표 (주간 합계)\n\
             - 커밋 24 · 저장소 6 · AI 68세션 · 회의 9건\n\
             \n\
             ⚠ 9/17 (목) 일지가 없어 빠졌습니다\n\
             ⚠ 9/16 — 섹션을 찾지 못해 원문 그대로 포함(지표 합계에서도 빠짐)\n\
             \n\
             _(9/16 편집본 기준)_\n\
             \n\
             ### 9/16 원문\n\
             \n\
             종일 회의.\n"
        );
    }

    #[test]
    fn parse_failure_warning_says_whether_metrics_were_counted() {
        // 지표 줄이 살아 있는 원문(v1 제목) — 합계에 들어갔다고 말한다.
        let k = RangeKpis {
            commits: 5,
            ..Default::default()
        };
        let counted = compose_weekly(&WeeklyParts {
            from: ymd(2026, 9, 14),
            to: ymd(2026, 9, 18),
            wins_md: "",
            decisions: &[],
            remaining: &[],
            done: 0,
            kpis: &k,
            missing: &[],
            parse_failures: &[(
                ymd(2026, 9, 16),
                "## 오늘 한 일\n- 손으로 적음\n\n## 📊 오늘 지표\n- 커밋 5".to_string(),
            )],
            edited: &[],
            llm_used: false,
        });
        assert!(counted.contains("⚠ 9/16 — 섹션을 찾지 못해 원문 그대로 포함(지표는 합산)"));
        assert!(!counted.contains("빠짐"));
        assert!(counted.contains("## 지표 (주간 합계)\n- 커밋 5\n"));

        // 지표 줄까지 없는 원문 — 합계에서도 빠졌다고 말한다.
        let empty = RangeKpis::default();
        let dropped = compose_weekly(&WeeklyParts {
            from: ymd(2026, 9, 14),
            to: ymd(2026, 9, 18),
            wins_md: "",
            decisions: &[],
            remaining: &[],
            done: 0,
            kpis: &empty,
            missing: &[],
            parse_failures: &[(ymd(2026, 9, 16), "종일 회의.".to_string())],
            edited: &[],
            llm_used: false,
        });
        assert!(
            dropped.contains("⚠ 9/16 — 섹션을 찾지 못해 원문 그대로 포함(지표 합계에서도 빠짐)")
        );
        assert!(!dropped.contains("지표는 합산"));
    }

    #[test]
    fn compose_weekly_empty_week_and_year_boundary() {
        let k = RangeKpis::default();
        let empty = WeeklyParts {
            from: ymd(2026, 9, 14),
            to: ymd(2026, 9, 18),
            wins_md: "",
            decisions: &[],
            remaining: &[],
            done: 0,
            kpis: &k,
            missing: &[],
            parse_failures: &[],
            edited: &[],
            llm_used: false,
        };
        let md = compose_weekly(&empty);
        assert!(md.contains("## 이번 주 성과\n- (없음)\n"));
        assert!(md.contains("## 결정 · 요청\n- (없음)\n"));
        assert!(md.contains("## 남은 할 일 (0)\n- (없음)\n"));
        assert!(md.contains("## 지표 (주간 합계)\n- 기록된 지표 없음\n"));
        assert!(md.ends_with("_중복 병합(LLM)을 쓰지 않아 날짜별 줄을 그대로 합쳤습니다._\n"));
        assert!(!md.contains("완료 0건") && !md.contains("⚠") && !md.contains("편집본"));

        // 해가 바뀌면 끝 날짜도 온전히 쓴다.
        let md = compose_weekly(&WeeklyParts {
            from: ymd(2025, 12, 29),
            to: ymd(2026, 1, 2),
            ..empty
        });
        assert!(md.starts_with("# 주간 업무일지 2025-12-29 ~ 2026-01-02\n"));
    }

    #[test]
    fn ids_names_and_resolve() {
        let all = all();
        assert_eq!(
            all.iter().map(|t| t.id).collect::<Vec<_>>(),
            TEMPLATE_IDS.to_vec()
        );
        assert_eq!(
            all.iter().map(|t| t.name).collect::<Vec<_>>(),
            vec!["표준", "보고용", "회고용"]
        );
        assert!(all.iter().all(|t| !t.description.is_empty()));

        for id in TEMPLATE_IDS {
            assert_eq!(resolve(id), id);
        }
        assert_eq!(resolve(" Retro "), "retro");
        assert_eq!(resolve("REPORT"), "report");
        assert_eq!(resolve("없는거"), DEFAULT_TEMPLATE);
        assert_eq!(resolve(""), DEFAULT_TEMPLATE);
        assert_eq!(DEFAULT_TEMPLATE, "standard");
    }

    #[test]
    fn info_sections_are_in_document_order() {
        let by = |id: &str| all().into_iter().find(|t| t.id == id).unwrap().sections;
        assert_eq!(
            by("standard"),
            vec![
                "오늘의 성과",
                "결정 · 요청 · 할 일",
                "프로젝트별 진행",
                "시간대별 흐름",
                "지표",
                "타임라인"
            ]
        );
        assert_eq!(
            by("report"),
            vec!["오늘의 성과", "결정 · 요청 · 할 일", "지표"]
        );
        assert_eq!(
            by("retro"),
            vec![
                "시간대별 흐름",
                "프로젝트별 진행",
                "막힌 것 · 배운 것",
                "결정 · 요청 · 할 일",
                "지표",
                "타임라인"
            ]
        );
    }

    #[test]
    fn system_prompt_lists_every_section_of_that_template() {
        for t in all() {
            let p = system_prompt(t.id);
            // 자동요약 세션 판별 서명(render::META_INTRO_SIGS)과 첫 문장이 일치해야 한다.
            assert!(p.starts_with("너는 하루치 개발 활동"), "{}", t.id);
            assert!(p.contains("규칙:\n1. 세션의 '질답 흐름'"), "{}", t.id);
            assert!(p.contains("메모"), "{}", t.id);
            assert!(p.contains("출력 구조:"), "{}", t.id);
            assert!(p.contains("인용문 한 줄"), "{}", t.id);
            assert!(p.ends_with("전체가 한눈에 들어오게. 문단 쓰지 말고 불릿만 써라."));
            for title in t
                .sections
                .iter()
                .filter(|s| **s != "지표" && **s != "타임라인")
            {
                assert!(p.contains(&format!("`## {title}`")), "{} / {title}", t.id);
            }
        }
        // 템플릿마다 없는 섹션은 프롬프트에도 없다.
        assert!(!system_prompt("report").contains("시간대별 흐름"));
        assert!(!system_prompt("report").contains("프로젝트별 진행"));
        assert!(!system_prompt("standard").contains("막힌 것"));
        // 상세 정도가 다르다(표준은 압축, 회고는 근거까지).
        assert!(system_prompt("standard").contains("블록당 2~4줄로 압축"));
        assert!(system_prompt("retro").contains("어느 세션·어느 커밋인지"));
        // 모르는 id 는 표준 프롬프트.
        assert_eq!(system_prompt("없는거"), system_prompt("standard"));
    }

    /// N3 — 근거 앵커 · 커밋 0일 완료 단정 금지 · 메모 표기가 세 템플릿 모두의 공통 규칙에 있다.
    #[test]
    fn system_prompt_has_grounding_rules() {
        for t in all() {
            let p = system_prompt(t.id);
            // (a) 근거 앵커 — 네 가지 근거 예시가 모두 프롬프트에 있다.
            assert!(p.contains("7. 근거 앵커"), "{}", t.id);
            for anchor in ["(a1b2c3d)", "(auth.py)", "(회의: 코드리뷰)", "(메모 10:35)"] {
                assert!(p.contains(anchor), "{} / {anchor}", t.id);
            }
            assert!(
                p.contains("근거를 찾을 수 없는 줄은 아예 쓰지 마라"),
                "{}",
                t.id
            );
            // (b) 커밋 0건인 날은 완료 단정 금지 — 판단 근거(Git 섹션 유무 · 가용 데이터 줄)까지 명시.
            assert!(p.contains("8. 커밋 0건인 날"), "{}", t.id);
            assert!(p.contains("'## Git 커밋' 섹션이 없거나"), "{}", t.id);
            assert!(p.contains("`Git ✅(N)`"), "{}", t.id);
            assert!(
                p.contains("'완료'·'마무리'·'확정' 같은 완료 단정을 쓰지 말고 '진행'·'검토'·'논의'·'초안' 으로 서술한다"),
                "{}",
                t.id
            );
            // (b-2) 범위·단계 숫자도 근거가 말한 만큼만 — 규칙 8 에 이어 붙인다.
            assert!(
                p.contains(
                    "진행 범위·단계 숫자('N단계 완료'·'전부 이식'·'모두 마침' 같은 범위 단정)"
                ),
                "{}",
                t.id
            );
            // (b-3) 단계 번호 자체도 그날 데이터에 그 번호가 보일 때만 — 규칙 8 의 마지막 문장.
            assert!(
                p.contains("단계·순번 번호('N단계', '#N', 'Phase N')"),
                "{}",
                t.id
            );
            // (c) 메모 유래 줄 표기 — Journal.tsx memoChips() 가 칩으로 렌더하는 네 가지 형태.
            assert!(p.contains("9. 메모에서 나온 줄"), "{}", t.id);
            for chip in ["[메모 · 요청]", "[메모 · 결정]", "[메모 · 할일]", "[메모]"]
            {
                assert!(p.contains(chip), "{} / {chip}", t.id);
            }
        }
    }

    /// 근거 앵커는 성과·진행 섹션에, 메모 표기는 결정·흐름 섹션 지시문에 각각 들어간다.
    #[test]
    fn section_instructions_carry_anchor_and_memo_marks() {
        // 성과 — 표준·보고용 둘 다.
        for id in ["standard", "report"] {
            let p = system_prompt(id);
            assert!(p.contains("규칙 7 대로 줄 끝에 근거 괄호"), "{id}");
            assert!(
                p.contains("커밋 0건인 날은 규칙 8 대로 완료 단정을 쓰지 않는다"),
                "{id}"
            );
        }
        // 프로젝트별 진행 — 표준·회고용(보고용에는 이 섹션이 없다).
        for id in ["standard", "retro"] {
            let p = system_prompt(id);
            assert!(p.contains("`내용 (근거) (상태)` 형식"), "{id}");
            assert!(p.contains("커밋 0건인 날은 (완료) 를 쓰지 않는다"), "{id}");
        }
        // 결정 · 요청 · 할 일 — 세 템플릿 모두에 있는 섹션.
        for id in TEMPLATE_IDS {
            assert!(
                system_prompt(id).contains(
                    "`[메모 · 요청]`/`[메모 · 결정]`/`[메모 · 할일]` 표기를 줄 끝에 붙인다"
                ),
                "{id}"
            );
        }
        // 시간대별 흐름 — 표준(압축)·회고(상세) 둘 다 메모 표기를 요구한다.
        for id in ["standard", "retro"] {
            assert!(
                system_prompt(id)
                    .contains("메모에서 온 줄에는 규칙 9 의 `[메모 · …]` 표기를 붙인다"),
                "{id}"
            );
        }
        // 보고용에는 흐름 섹션 자체가 없다.
        assert!(!system_prompt("report").contains("규칙 9 의 `[메모 · …]`"));
    }

    #[test]
    fn compose_standard_has_title_llm_body_and_facts() {
        let tz = get_tz("Asia/Seoul");
        let md = compose(
            "standard",
            day(),
            Some("  > 한 줄 요약\n\n## 오늘의 성과\n- [repoA] 큰 기능 완료  "),
            None,
            &sample_analysis(),
            tz,
            "# 원본 사실 데이터",
            false,
        );
        assert!(md.starts_with("# 업무일지 2026-07-28 (화)\n\n> 한 줄 요약\n"));
        assert!(md.contains("## 오늘의 성과\n- [repoA] 큰 기능 완료\n"));
        assert!(md.contains(
            "## 지표\n- 커밋 **2** (+403/−3) · 저장소 1 · AI **1세션** · 출력 5K토큰 · 활동 09:00–12:00\n"
        ));
        assert!(md.contains("**프로젝트별 집중**\n\n| 프로젝트 | 집중시간 |"));
        assert!(md.contains("<details>\n<summary>타임라인</summary>\n\n- `09:00–10:30` 🤖"));
        assert!(md.contains("\n</details>\n"));
        // 옛 문서 형식의 흔적은 남지 않는다.
        assert!(!md.contains("핵심 성과"));
        assert!(!md.contains("📝 업무일지") && !md.contains("📊 오늘 지표"));
        assert!(!md.contains("수집 데이터 원본"));
        assert!(md.ends_with("\n") && !md.ends_with("\n\n"));
    }

    #[test]
    fn compose_report_is_metrics_line_only_and_retro_matches_standard() {
        let tz = get_tz("Asia/Seoul");
        let a = sample_analysis();
        let rep = compose("report", day(), Some("> 요약"), None, &a, tz, "", false);
        assert!(rep.starts_with("# 업무일지 2026-07-28 (화)\n\n> 요약\n\n## 지표\n- 커밋 **2**"));
        assert!(!rep.contains("프로젝트별 집중"));
        assert!(!rep.contains("타임라인"));
        assert!(!rep.contains("<details>"));

        let retro = compose("retro", day(), Some("> 요약"), None, &a, tz, "", false);
        let standard = compose("standard", day(), Some("> 요약"), None, &a, tz, "", false);
        assert_eq!(retro, standard); // 결정론적 부분은 표준과 같다
    }

    #[test]
    fn compose_without_summary_and_with_raw_appendix() {
        let tz = get_tz("Asia/Seoul");
        let md = compose(
            "standard",
            day(),
            None,
            None,
            &Analysis::default(),
            tz,
            "# 원본 사실 데이터\n- 커밋 목록",
            true,
        );
        assert!(md.contains("> LLM 요약을 사용하지 않았습니다. 아래 지표를 참고하세요."));
        // 데이터가 없어도 지표 한 줄은 늘 남는다 — 다만 0 을 나열하지는 않는다(N4).
        assert!(md.contains("## 지표\n- 기록된 지표 없음\n"));
        assert!(!md.contains("커밋 **0**"));
        assert!(!md.contains("프로젝트별 집중") && !md.contains("<summary>타임라인"));
        assert!(md.contains(
            "---\n\n<details>\n<summary>수집 데이터 원본</summary>\n\n# 원본 사실 데이터\n- 커밋 목록\n\n</details>\n"
        ));
        // 빈 요약 문자열도 안내 문구로.
        let blank = compose(
            "report",
            day(),
            Some("   "),
            None,
            &Analysis::default(),
            tz,
            "",
            false,
        );
        assert!(blank.contains("> LLM 요약을 사용하지 않았습니다."));
    }

    /// V2 — 요약 실패는 '사용하지 않았습니다' 와 다른 문장으로 남는다(안 한 날 ≠ 실패한 날).
    #[test]
    fn compose_marks_summary_failure_and_partial_summary() {
        let tz = get_tz("Asia/Seoul");
        let a = sample_analysis();

        // (a) 요약 없음 + 사유 있음 — 경고 한 줄.
        let failed = compose(
            "standard",
            day(),
            None,
            Some("claude CLI 시간 초과(600초)"),
            &a,
            tz,
            "",
            false,
        );
        assert!(failed.contains(
            "> ⚠ AI 요약 시도가 실패했습니다: claude CLI 시간 초과(600초). 아래는 수집 데이터만 정리한 것입니다."
        ));
        assert!(!failed.contains("LLM 요약을 사용하지 않았습니다"));

        // (b) 요약을 쓰지 않은 날은 예전 문구 그대로 — 두 문서가 서로 달라야 한다.
        let skipped = compose("standard", day(), None, None, &a, tz, "", false);
        assert!(skipped.contains("> LLM 요약을 사용하지 않았습니다. 아래 지표를 참고하세요."));
        assert!(!skipped.contains("⚠"));
        assert_ne!(failed, skipped);

        // (c) 부분 요약 — 본문은 살리고 바로 아래 한 줄.
        let partial = compose(
            "standard",
            day(),
            Some("> 한 줄 요약\n\n## 오늘의 성과\n- [repoA] 큰 기능 (a1b2c3d)"),
            Some("claude CLI 시간 초과(600초)"),
            &a,
            tz,
            "",
            false,
        );
        assert!(partial.starts_with("# 업무일지 2026-07-28 (화)\n\n> 한 줄 요약\n"));
        assert!(partial.contains("- [repoA] 큰 기능 (a1b2c3d)"));
        assert!(partial.contains("\n> ⚠ 요약 일부만 생성됨: claude CLI 시간 초과(600초)\n"));
        // 경고는 지표 위, 본문 아래에.
        let warn = partial.find("> ⚠ 요약 일부만").unwrap();
        assert!(warn < partial.find("## 지표").unwrap());
        assert!(warn > partial.find("## 오늘의 성과").unwrap());

        // (d) 빈 사유 문자열은 사유 없음과 같게 본다.
        let blank = compose("standard", day(), None, Some("   "), &a, tz, "", false);
        assert!(blank.contains("> LLM 요약을 사용하지 않았습니다."));
        assert!(!blank.contains("⚠"));
    }

    /// V2 — 규칙 7: 앵커는 그날 프롬프트에 들어온 데이터에서만. (해시를 지어내던 건)
    #[test]
    fn rule_seven_binds_anchors_to_todays_data() {
        for t in all() {
            let p = system_prompt(t.id);
            assert!(
                p.contains("근거는 **이 프롬프트에 들어온 오늘 데이터 안에 있는 것만** 쓴다"),
                "{}",
                t.id
            );
            assert!(
                p.contains("'## Git 커밋' 목록(오늘 커밋)에 실제로 적힌 해시만 그대로 옮긴다"),
                "{}",
                t.id
            );
            assert!(
                p.contains(
                    "기억·추측으로 해시를 지어내지 말고, 다른 날·다른 저장소의 해시를 끌어오지 마라"
                ),
                "{}",
                t.id
            );
            // 커밋이 없으면 파일명·회의·메모로, 그것도 없으면 쓰지 않는다.
            assert!(
                p.contains("오늘 커밋 목록이 없거나 그 줄에 맞는 해시가 없으면 해시 대신"),
                "{}",
                t.id
            );
            assert!(
                p.contains("중 **오늘 데이터에 있는 것** 하나를 쓴다"),
                "{}",
                t.id
            );
            assert!(
                p.contains("근거를 찾을 수 없는 줄은 아예 쓰지 마라"),
                "{}",
                t.id
            );
            // 규칙 번호는 그대로 7·8·9.
            assert!(
                p.contains("7. 근거 앵커") && p.contains("8. 커밋 0건인 날"),
                "{}",
                t.id
            );
            assert!(p.contains("9. 메모에서 나온 줄"), "{}", t.id);
        }
    }

    /// V2 — 규칙 8: 범위·단계 숫자는 근거가 말한 만큼만. (09-14 판이 커밋 0~5단계를 두고
    /// '0~8단계 이식 완료' 라고 적었고, 세션에는 7단계가 '앞으로 할 일' 로 남아 있었다.)
    #[test]
    fn rule_eight_bounds_scope_claims() {
        for t in all() {
            let p = system_prompt(t.id);
            // 규칙 번호는 그대로 8 — 새 규칙을 만들지 않고 이어 붙였다.
            assert!(p.contains("8. 커밋 0건인 날"), "{}", t.id);
            assert!(!p.contains("10. "), "{}", t.id);
            assert!(
                p.contains("커밋 메시지나 사용자 발화가 **같은 범위를 말할 때만** 쓴다"),
                "{}",
                t.id
            );
            assert!(
                p.contains("근거가 그보다 좁으면 근거에 적힌 범위 그대로 적는다"),
                "{}",
                t.id
            );
            // 09-14 사고의 실제 문장을 반례로 박아 둔다.
            assert!(
                p.contains("'0~8단계 이식 완료' 가 아니라 '4단계 완료 · 5단계 진행 중' 으로"),
                "{}",
                t.id
            );
            assert!(
                p.contains("남은 단계를 완료 쪽으로 올려 묶지 마라"),
                "{}",
                t.id
            );
            // V3 — 범위뿐 아니라 '번호' 자체도 근거에 보일 때만. (09-20 판이 그날 커밋·발화에
            // 없는 '7단계' 를 붙여 '7단계(… 세 화면 구현) 진행 중' 이라고 적었다. 그날 계획표는
            // 세 화면을 6단계에 두고 있었다.)
            assert!(
                p.contains("단계·순번 번호('N단계', '#N', 'Phase N')"),
                "{}",
                t.id
            );
            assert!(
                p.contains("**그 번호가 실제로 보일 때만** 쓴다"),
                "{}",
                t.id
            );
            assert!(
                p.contains("보이지 않으면 번호 없이 작업 내용만 적고, 번호를 추측해 붙이지 마라"),
                "{}",
                t.id
            );
        }
    }

    #[test]
    fn weekday_letters() {
        let d = |day| NaiveDate::from_ymd_opt(2026, 7, day).unwrap();
        // 2026-07-27 은 월요일.
        assert_eq!(
            (27..=31).map(|n| weekday_ko(d(n))).collect::<Vec<_>>(),
            vec!["월", "화", "수", "목", "금"]
        );
        assert_eq!(weekday_ko(d(25)), "토");
        assert_eq!(weekday_ko(d(26)), "일");
        for n in 1..=31 {
            assert!(
                compose(
                    "standard",
                    d(n),
                    None,
                    None,
                    &Analysis::default(),
                    get_tz("Asia/Seoul"),
                    "",
                    false
                )
                .starts_with(&format!("# 업무일지 2026-07-{n:02} ({})", weekday_ko(d(n))))
            );
        }
    }
}
