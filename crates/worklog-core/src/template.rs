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
};

pub const DEFAULT_TEMPLATE: &str = "standard";
pub const TEMPLATE_IDS: [&str; 3] = ["standard", "report", "retro"];

// --------------------------------------------------------------------------- //
// 섹션 제목 · 지시문
// --------------------------------------------------------------------------- //

const T_WINS: &str = "오늘의 성과";
const T_DECISIONS: &str = "결정 · 요청 · 할 일";
const T_PROJECTS: &str = "프로젝트별 진행";
const T_FLOW: &str = "시간대별 흐름";
const T_LEARNED: &str = "막힌 것 · 배운 것";

/// 결정론적 섹션 제목(LLM 이 쓰지 않는다).
const T_METRICS: &str = "지표";
const T_TIMELINE: &str = "타임라인";

const I_WINS: &str = concat!(
    "3~6줄. 그날 실제로 끝낸 결과만. 프로젝트를 알 수 있으면 각 줄을 [프로젝트] 태그로 시작하고, ",
    "주간보고에 그대로 옮겨 붙일 수 있는 문장으로 쓴다."
);
const I_DECISIONS: &str = concat!(
    "결정은 \"결정: …\", 구두 요청은 \"요청(@이름): …\", 앞으로 할 일은 체크박스 \"- [ ] …\" 로 쓴다. ",
    "'메모'(사용자가 직접 남긴 1차 사실)는 빠짐없이 반영하고, 세션·커밋에서 드러난 결정도 포함한다. ",
    "해당하는 것이 하나도 없으면 \"- (없음)\" 한 줄만."
);
const I_PROJECTS: &str = concat!(
    "프로젝트마다 **굵은 이름** 아래 2~4줄. 각 줄 끝에 상태를 (완료) · (진행 중) · (막힘) 중 하나로 붙인다. ",
    "한 프로젝트를 여러 세션에서 다뤘으면 합쳐서 정리한다."
);
const I_FLOW_BRIEF: &str = concat!(
    "**굵은 시간대**(예: **09–12시**) 블록으로 묶고 블록당 2~4줄로 압축한다. ",
    "회의는 반드시 해당 시각에 명시한다."
);
const I_FLOW_FULL: &str = concat!(
    "**굵은 시간대** 블록으로 묶어 시각과 근거(어느 세션·어느 커밋인지)까지 자세히 쓴다. ",
    "회의는 반드시 해당 시각에 명시한다."
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
pub fn compose(
    id: &str,
    target: NaiveDate,
    summary: Option<&str>,
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
    match summary.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => lines.push(s.to_string()),
        None => lines.push("> LLM 요약을 사용하지 않았습니다. 아래 지표를 참고하세요.".into()),
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
    "요청자(@이름)를 그대로 명시한다.\n\n",
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

    #[test]
    fn compose_standard_has_title_llm_body_and_facts() {
        let tz = get_tz("Asia/Seoul");
        let md = compose(
            "standard",
            day(),
            Some("  > 한 줄 요약\n\n## 오늘의 성과\n- [repoA] 큰 기능 완료  "),
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
        let rep = compose("report", day(), Some("> 요약"), &a, tz, "", false);
        assert!(rep.starts_with("# 업무일지 2026-07-28 (화)\n\n> 요약\n\n## 지표\n- 커밋 **2**"));
        assert!(!rep.contains("프로젝트별 집중"));
        assert!(!rep.contains("타임라인"));
        assert!(!rep.contains("<details>"));

        let retro = compose("retro", day(), Some("> 요약"), &a, tz, "", false);
        let standard = compose("standard", day(), Some("> 요약"), &a, tz, "", false);
        assert_eq!(retro, standard); // 결정론적 부분은 표준과 같다
    }

    #[test]
    fn compose_without_summary_and_with_raw_appendix() {
        let tz = get_tz("Asia/Seoul");
        let md = compose(
            "standard",
            day(),
            None,
            &Analysis::default(),
            tz,
            "# 원본 사실 데이터\n- 커밋 목록",
            true,
        );
        assert!(md.contains("> LLM 요약을 사용하지 않았습니다. 아래 지표를 참고하세요."));
        // 데이터가 없어도 지표 한 줄은 늘 남는다.
        assert!(
            md.contains("## 지표\n- 커밋 **0** (+0/−0) · 저장소 0 · AI **0세션** · 출력 0토큰\n")
        );
        assert!(!md.contains("프로젝트별 집중") && !md.contains("<summary>타임라인"));
        assert!(md.contains(
            "---\n\n<details>\n<summary>수집 데이터 원본</summary>\n\n# 원본 사실 데이터\n- 커밋 목록\n\n</details>\n"
        ));
        // 빈 요약 문자열도 안내 문구로.
        let blank = compose(
            "report",
            day(),
            Some("   "),
            &Analysis::default(),
            tz,
            "",
            false,
        );
        assert!(blank.contains("> LLM 요약을 사용하지 않았습니다."));
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
