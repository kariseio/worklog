//! LLM 종합(요약)기.
//!
//! 수집한 '정제 신호'를 Claude 에게 주고 자연어 업무일지로 다듬는다.
//! provider:
//!   auto          → claude CLI 있으면 사용, 없으면 Anthropic API(ANTHROPIC_API_KEY), 둘 다 없으면 None
//!   claude_cli    → 설치된 `claude` CLI (별도 API 키 불필요)
//!   anthropic_api → Anthropic Messages API
//!   none          → 요약 생략
//!
//! 무거운 날(세션 질답이 많은 날)은 세션별로 먼저 압축(map)한 뒤 종합(reduce)한다.

use std::{
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    config::{self, SummarizerConfig},
    render::{SESSION_SECTION_HEADER, WORKLOG_SENTINEL},
    template,
};

/// 무거운 날: 세션마다 먼저 이걸로 개별 압축(map) 후, 압축본을 모아 템플릿 프롬프트로 종합(reduce).
/// 템플릿과 무관한 공통 압축 프롬프트다.
pub const CONDENSE_SYSTEM_KO: &str = concat!(
    "너는 Claude Code 한 세션의 '질답 흐름'을 요약하는 도구다. ",
    "이 세션에서 사용자가 무엇을 요청·논의했고 무엇이 결정·완료됐는지를 시간 흐름을 살려 ",
    "3~8줄 개조식으로 압축하라. 여러 주제를 다뤘으면 주제별로 한 줄씩. ",
    "장황체·미사여구 금지, 결과·결정 중심. 완료된 변경은 파일/커밋 근거로 명확히. ",
    "문단 쓰지 말고 불릿만."
);

pub fn user_prompt(date_iso: &str, signal: &str, availability: &str) -> String {
    let availability = if availability.is_empty() {
        "가용 데이터: (표기 없음)"
    } else {
        availability
    };
    format!(
        "{availability}\n\n아래는 {date_iso} 활동 데이터(정제 신호 + 시간순 이벤트 + 세션 질답 흐름)다. 위 규칙대로 \
         '시간대별 흐름'과 '프로젝트별 정리'를 담은 업무일지 본문만 출력하라. 원문을 그대로 나열하지 마라.\n\n\
         ---\n{signal}\n---\n"
    )
}

/// `summarizer.model` 이 비어 있을 때 Anthropic API 로 보낼 기본 모델.
/// (claude CLI 쪽은 `--model` 자체를 생략해 CLI 기본 모델에 맡긴다.)
pub const DEFAULT_API_MODEL: &str = "claude-opus-5";

/// 설정의 모델 이름. 비어 있거나 공백뿐이면 `None` = 제공자 기본값을 쓴다.
fn configured_model(cfg: &SummarizerConfig) -> Option<&str> {
    let m = cfg.model.trim();
    (!m.is_empty()).then_some(m)
}

/// 세션별 압축(map) 한 번 호출 상한의 천장(초). 종합(reduce)은 설정값 전체를 쓰고,
/// map 은 `min(설정값, 이 값)` — 조각 하나가 오래 물고 있어도 하루 전체가 끝나게.
pub const MAP_TIMEOUT_CAP_SECS: u64 = 300;

/// 이번 호출의 상한. 설정값(`summarizer.timeout_secs`, 기본 600초)을 쓰되
/// 손으로 만든 설정이 0 이어도 최소값 아래로는 내려가지 않게 조인다.
fn call_timeout(cfg: &SummarizerConfig) -> Duration {
    Duration::from_secs(
        cfg.timeout_secs
            .clamp(config::SUMMARY_TIMEOUT_MIN, config::SUMMARY_TIMEOUT_MAX),
    )
}

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    None,
    ClaudeCli,
    AnthropicApi,
}

/// PATH 에서 `claude` 실행 파일을 한 번만 찾는다(map-reduce 로 수십 번 불려도 PATH 스캔 1회).
pub fn claude_exe() -> Option<PathBuf> {
    static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();
    EXE.get_or_init(find_claude_exe).clone()
}

fn find_claude_exe() -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["claude.cmd", "claude.exe", "claude.bat", "claude"]
    } else {
        &["claude"]
    };
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for n in names {
            let p = dir.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn anthropic_available() -> bool {
    std::env::var_os("ANTHROPIC_API_KEY").is_some_and(|v| !v.is_empty())
}

/// 설정 문자열 → 실제 provider. 모르는 값은 경고 후 None.
pub fn resolve_provider(provider: &str) -> Provider {
    match provider.trim() {
        "auto" => {
            if claude_exe().is_some() {
                Provider::ClaudeCli
            } else if anthropic_available() {
                Provider::AnthropicApi
            } else {
                tracing::warn!(
                    "요약기: claude CLI 도 ANTHROPIC_API_KEY 도 없어 요약을 건너뜁니다."
                );
                Provider::None
            }
        }
        "none" => Provider::None,
        "claude_cli" => Provider::ClaudeCli,
        "anthropic_api" => Provider::AnthropicApi,
        other => {
            tracing::warn!(
                "알 수 없는 summarizer.provider {other:?} → 요약을 건너뜁니다. (auto | none | claude_cli | anthropic_api)"
            );
            Provider::None
        }
    }
}

/// (system, user) 한 번 호출. 테스트에서 가짜로 바꿔 끼운다.
pub trait LlmCaller: Send + Sync {
    /// `cancel` 이 켜지면 진행 중인 호출을 가능한 한 빨리 끊고 None 을 돌려준다.
    fn call(
        &self,
        system: &str,
        user: &str,
        cfg: &SummarizerConfig,
        cancel: Option<&AtomicBool>,
    ) -> Option<String>;

    /// [`call`](Self::call) 과 같되 **실패 사유**를 돌려준다(문서에 한 줄로 적힌다).
    /// 기본 구현은 사유 없이 감싸기만 한다 — 실제 호출기만 사유를 채운다.
    fn call_detailed(
        &self,
        system: &str,
        user: &str,
        cfg: &SummarizerConfig,
        cancel: Option<&AtomicBool>,
    ) -> Result<String, String> {
        self.call(system, user, cfg, cancel)
            .ok_or_else(|| "요약 실패(사유 없음)".to_string())
    }
}

/// provider 가 없어(설치·키 없음, 모르는 값) 호출조차 못 했을 때의 사유.
pub const NO_SUMMARIZER: &str = "요약기 없음 — claude CLI 도 ANTHROPIC_API_KEY 도 없습니다";

/// 실제 provider 로 호출.
pub struct RealCaller;

impl LlmCaller for RealCaller {
    fn call(
        &self,
        system: &str,
        user: &str,
        cfg: &SummarizerConfig,
        cancel: Option<&AtomicBool>,
    ) -> Option<String> {
        self.call_detailed(system, user, cfg, cancel).ok()
    }

    fn call_detailed(
        &self,
        system: &str,
        user: &str,
        cfg: &SummarizerConfig,
        cancel: Option<&AtomicBool>,
    ) -> Result<String, String> {
        let r = match resolve_provider(&cfg.provider) {
            Provider::ClaudeCli => call_claude_cli(system, user, cfg, cancel),
            Provider::AnthropicApi => call_anthropic_api(system, user, cfg),
            Provider::None => Err(NO_SUMMARIZER.to_string()),
        };
        if let Err(e) = &r {
            tracing::warn!("요약 호출 실패: {e}");
        }
        r
    }
}

/// 자식 프로세스에 stdin 을 넣고 stdout 을 받되, 상한 시간이 지나거나 취소되면 죽인다.
fn run_with_timeout(
    mut cmd: Command,
    input: &str,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> Result<(i32, String, String), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW); // 창(--windowed) 앱에서 콘솔이 깜빡이지 않게
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().ok_or("stdin")?;
    let mut stdout = child.stdout.take().ok_or("stdout")?;
    let mut stderr = child.stderr.take().ok_or("stderr")?;
    let input = input.to_string();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
        // drop → EOF
    });
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("취소됨".into());
                }
                if started.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("시간 초과({}초)", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let _ = writer.join();
    let out = out_reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    Ok((status.code().unwrap_or(-1), out, err))
}

fn call_claude_cli(
    system: &str,
    user: &str,
    cfg: &SummarizerConfig,
    cancel: Option<&AtomicBool>,
) -> Result<String, String> {
    let Some(exe) = claude_exe() else {
        return Err("claude CLI 를 찾을 수 없습니다".into());
    };
    // 이 요약 호출이 만드는 Claude 세션을 나중에 확실히 걸러내기 위한 표식(맨 앞).
    let full = format!("{WORKLOG_SENTINEL}\n{system}\n\n{user}");
    let mut cmd = Command::new(exe);
    cmd.arg("-p");
    // 모델을 비워 뒀으면 `--model` 을 붙이지 않는다 — claude CLI 의 기본 모델을 그대로 쓴다.
    if let Some(model) = configured_model(cfg) {
        cmd.args(["--model", model]);
    }
    match run_with_timeout(cmd, &full, call_timeout(cfg), cancel) {
        Ok((0, out, _)) => {
            let out = out.trim();
            if out.is_empty() {
                return Err("claude CLI 응답이 비어 있습니다".into());
            }
            Ok(out.to_string())
        }
        Ok((code, _, err)) => Err(format!("claude CLI 오류(exit {code}): {}", snippet(&err))),
        // 시간 초과·취소 사유는 `run_with_timeout` 이 이미 사람이 읽을 문장으로 만들어 준다.
        Err(e) => Err(format!("claude CLI {e}")),
    }
}

/// 오류 본문은 문서 한 줄에 들어갈 만큼만.
fn snippet(s: &str) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= 160 {
        return one;
    }
    format!("{}…", one.chars().take(159).collect::<String>())
}

fn call_anthropic_api(system: &str, user: &str, cfg: &SummarizerConfig) -> Result<String, String> {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|k| !k.is_empty());
    let Some(key) = key else {
        return Err("API 오류: ANTHROPIC_API_KEY 가 없습니다".into());
    };
    let client = reqwest::blocking::Client::builder()
        .timeout(call_timeout(cfg))
        .build()
        .map_err(|e| format!("API 오류: {e}"))?;
    let body = serde_json::json!({
        // 모델을 비워 뒀으면 API 에는 빈 값을 보낼 수 없으므로 기본 모델을 쓴다.
        "model": configured_model(cfg).unwrap_or(DEFAULT_API_MODEL),
        "max_tokens": cfg.max_tokens,
        "system": system,
        "messages": [{"role": "user", "content": user}],
    });
    let resp = client
        .post(ANTHROPIC_URL)
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send();
    let resp = match resp {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            return Err(format!(
                "API 오류: 시간 초과({}초)",
                call_timeout(cfg).as_secs()
            ));
        }
        Err(e) => return Err(format!("API 오류: {e}")),
    };
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "API 오류(HTTP {}): {}",
            status.as_u16(),
            snippet(&text)
        ));
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("API 오류: 응답 해석 실패({e})"))?;
    let out: String = v
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let out = out.trim();
    if out.is_empty() {
        return Err("API 오류: 응답이 비어 있습니다".into());
    }
    Ok(out.to_string())
}

// --------------------------------------------------------------------------- //
// 요약기
// --------------------------------------------------------------------------- //

/// 진행 상황 콜백: ("단계", "세부"). 예: ("요약", "세션 3/7").
pub type ProgressFn = dyn Fn(&str, &str) + Send + Sync;

/// 요약 한 번의 결과 — **'안 했다' 와 '하려다 실패했다' 를 구분**하려고 사유를 함께 돌려준다.
///
/// * `text: Some`, `error: None` — 정상.
/// * `text: None`, `error: Some` — 시도했지만 실패(문서에 경고 한 줄이 남는다).
/// * `text: Some`, `error: Some` — **부분 요약**(map 은 됐고 reduce 가 실패).
/// * 둘 다 `None` — 사용자가 요약을 끈 것(`provider = none`, `--no-llm`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SummaryOutcome {
    pub text: Option<String>,
    pub error: Option<String>,
}

impl SummaryOutcome {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            error: None,
        }
    }

    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            text: None,
            error: Some(error.into()),
        }
    }

    fn of(r: Result<String, String>) -> Self {
        match r {
            Ok(t) => Self::ok(t),
            Err(e) => Self::failed(e),
        }
    }
}

/// 병합(reduce)이 실패해 세션별 압축본만 붙였을 때 문서 맨 앞에 남는 표시.
pub const PARTIAL_HEADING: &str = "(부분 요약 — 병합 단계 실패, 세션별 요약을 그대로 붙임)";

/// 응답이 오긴 왔는데 요약이 아닐 때(인사말·거절문)의 실패 사유.
pub const IMPLAUSIBLE_SUMMARY: &str = "요약 응답이 요약이 아님(인사말/거절)";

/// 인사말·거절문 서명 — 소문자로 바꿔 부분 일치로 본다(한글 서명은 대소문자 영향 없음).
const BOILERPLATE: &[&str] = &[
    "i'll help you",
    "what would you like",
    "how can i help",
    "i can't help",
    "죄송",
    "도와드릴까요",
    "무엇을 도와",
];

/// 이 응답을 '요약' 으로 볼 수 있는가.
///
/// 강제 실패 실험(09-15)에서 조각 하나가
/// `I'll help you with your work. What would you like me to do?` 로 돌아와 그대로 문서에 붙었다.
/// 비지 않았다는 것만으로는 성공이 아니다 — 아래 중 하나라도 걸리면 **실패로 친다**(시간 초과와 동일 취급).
///   * 다듬은 길이가 20자 미만
///   * 한글이 하나도 없고 불릿·머리글 줄도 없음
///   * 인사말·거절문 서명([`BOILERPLATE`])이 들어 있음
pub fn plausible_summary(text: &str) -> bool {
    let t = text.trim();
    if t.chars().count() < 20 {
        return false;
    }
    let lower = t.to_lowercase();
    if BOILERPLATE.iter().any(|b| lower.contains(b)) {
        return false;
    }
    t.chars().any(is_hangul) || t.lines().any(is_bullet_or_heading)
}

/// 한글(음절·자모)인가.
fn is_hangul(c: char) -> bool {
    matches!(c, '\u{AC00}'..='\u{D7A3}' | '\u{1100}'..='\u{11FF}' | '\u{3130}'..='\u{318F}')
}

/// `- `·`*`·`•`·`#` 로 시작하거나 `1.`·`1)` 로 시작하는 줄(=개조식·머리글).
fn is_bullet_or_heading(line: &str) -> bool {
    let l = line.trim_start();
    let b = l.as_bytes();
    if matches!(b.first(), Some(b'-' | b'*' | b'#')) || l.starts_with('•') {
        return true;
    }
    let n = b.iter().take_while(|c| c.is_ascii_digit()).count();
    n > 0 && matches!(b.get(n), Some(b'.' | b')'))
}

/// 세션 압축(map)이 실패했을 때 부분 요약에 남길 질답 원문 줄 수 상한(세션당).
pub const VERBATIM_QA_LINES: usize = 8;

/// 압축이 실패한 세션의 폴백 본문 — 질답 원문(`- HH:MM Q: … → A: …`)을 앞에서 여덟 줄만 남기고
/// 나머지는 `- … 외 N개 질답 생략` 한 줄로 접는다(부분 요약 문서 전체 길이 제한).
fn verbatim_fallback(block: &str) -> String {
    let body = block_body(block);
    let lines: Vec<&str> = body
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    if lines.len() <= VERBATIM_QA_LINES {
        return lines.join("\n");
    }
    let omitted = lines.len() - VERBATIM_QA_LINES;
    format!(
        "{}\n- … 외 {omitted}개 질답 생략",
        lines[..VERBATIM_QA_LINES].join("\n")
    )
}

/// 세션별 압축본을 그대로 이어 붙인 '부분 요약' 본문.
fn partial_text(condensed: &[(String, String)]) -> String {
    let body = condensed
        .iter()
        .filter(|(_, s)| !s.trim().is_empty())
        .map(|(l, s)| format!("### {l}\n{}", s.trim()))
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("> {PARTIAL_HEADING}\n\n{body}")
}

pub struct Summarizer {
    cfg: SummarizerConfig,
    caller: Box<dyn LlmCaller>,
    progress: Option<Arc<ProgressFn>>,
    cancel: Option<Arc<AtomicBool>>,
}

impl Summarizer {
    pub fn new(cfg: SummarizerConfig) -> Self {
        Self {
            cfg,
            caller: Box::new(RealCaller),
            progress: None,
            cancel: None,
        }
    }

    pub fn with_caller(cfg: SummarizerConfig, caller: Box<dyn LlmCaller>) -> Self {
        Self {
            cfg,
            caller,
            progress: None,
            cancel: None,
        }
    }

    /// 단계별 진행 콜백(앱 화면의 "AI 요약 · 세션 3/7").
    pub fn with_progress(mut self, f: Arc<ProgressFn>) -> Self {
        self.progress = Some(f);
        self
    }

    /// 취소 플래그. 켜지면 다음 LLM 호출부터 건너뛴다(진행 중인 호출은 끝까지 기다림).
    pub fn with_cancel(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel = Some(flag);
        self
    }

    pub fn provider(&self) -> Provider {
        resolve_provider(&self.cfg.provider)
    }

    fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Relaxed))
    }

    fn report(&self, step: &str, detail: &str) {
        if let Some(p) = &self.progress {
            p(step, detail);
        }
    }

    /// 한 번 호출 — 실패 사유까지. 취소된 뒤에는 호출하지 않는다.
    fn call_detailed(
        &self,
        system: &str,
        user: &str,
        cfg: &SummarizerConfig,
    ) -> Result<String, String> {
        if self.cancelled() {
            return Err("취소됨".into());
        }
        self.caller
            .call_detailed(system, user, cfg, self.cancel.as_deref())
    }

    /// 종합(reduce)·단일 호출 — 설정 상한(`timeout_secs`)을 그대로 쓴다.
    fn call(&self, system: &str, user: &str) -> Option<String> {
        self.call_detailed(system, user, &self.cfg).ok()
    }

    /// 세션별 압축(map) 한 번 — 상한은 `min(설정값, `[`MAP_TIMEOUT_CAP_SECS`]`)`.
    fn call_map(&self, system: &str, user: &str) -> Result<String, String> {
        self.call_detailed(system, user, &self.map_cfg())
    }

    /// map 단계용 설정 사본 — 호출 상한만 줄인 것.
    fn map_cfg(&self) -> SummarizerConfig {
        let mut c = self.cfg.clone();
        c.timeout_secs = c.timeout_secs.min(MAP_TIMEOUT_CAP_SECS);
        c
    }

    /// 종합(reduce) 한 번 — 실패 사유까지.
    fn reduce(
        &self,
        system: &str,
        signal: &str,
        date_iso: &str,
        availability: &str,
    ) -> Result<String, String> {
        self.report("요약", "종합");
        let out = self.call_detailed(
            system,
            &user_prompt(date_iso, signal, availability),
            &self.cfg,
        )?;
        // 응답이 와도 요약이 아니면(인사말·거절) 실패다 — 그대로 문서에 붙이지 않는다.
        if !plausible_summary(&out) {
            tracing::warn!("종합(reduce) 응답이 요약이 아님: {}", snippet(&out));
            return Err(IMPLAUSIBLE_SUMMARY.to_string());
        }
        Ok(out)
    }

    /// 표준 템플릿으로 단일 호출 요약. provider 가 none 이면 None.
    pub fn summarize(&self, signal: &str, date_iso: &str, availability: &str) -> Option<String> {
        self.summarize_with(
            &template::system_prompt(template::DEFAULT_TEMPLATE),
            signal,
            date_iso,
            availability,
        )
    }

    /// 단일 호출 요약 — `system` 은 [`crate::template::system_prompt`] 결과.
    pub fn summarize_with(
        &self,
        system: &str,
        signal: &str,
        date_iso: &str,
        availability: &str,
    ) -> Option<String> {
        if self.provider() == Provider::None {
            tracing::info!("요약기: 사용 안 함 (수집 데이터만 정리)");
            return None;
        }
        self.reduce(system, signal, date_iso, availability).ok()
    }

    /// system · user 를 가공 없이 한 번만 호출한다.
    ///
    /// [`summarize_with`](Self::summarize_with) 은 입력을 '하루치 신호' 프롬프트로 감싸므로
    /// 하루가 아닌 입력(주간 중복 병합 — `docs/product-plan.md` N8)에는 이쪽을 쓴다.
    pub fn summarize_raw(&self, system: &str, user: &str) -> Option<String> {
        if self.provider() == Provider::None {
            tracing::info!("요약기: 사용 안 함 (합쳐 놓은 초안 그대로)");
            return None;
        }
        self.report("요약", "병합");
        self.call(system, user)
    }

    /// 표준 템플릿으로 하루 업무일지 생성. [`summarize_day_with`](Self::summarize_day_with) 의 얇은 껍데기.
    pub fn summarize_day(
        &self,
        signal: &str,
        date_iso: &str,
        availability: &str,
    ) -> Option<String> {
        self.summarize_day_with(
            &template::system_prompt(template::DEFAULT_TEMPLATE),
            signal,
            date_iso,
            availability,
        )
    }

    /// 하루 업무일지 생성. 신호가 크면(세션 질답이 많으면) map-reduce 로 안전 처리.
    ///
    /// 가벼우면 신호 그대로 단일 호출. 크면 세션 질답 섹션을 세션별로 쪼개 먼저 개별 요약(병렬)한 뒤,
    /// 압축본으로 신호를 재구성해 종합한다. (컨텍스트 초과·품질 희석 방지)
    /// 세션별 압축(map)은 템플릿과 무관한 [`CONDENSE_SYSTEM_KO`] 를 쓰고, 종합(reduce)에만
    /// 템플릿 프롬프트 `system` 을 쓴다.
    pub fn summarize_day_with(
        &self,
        system: &str,
        signal: &str,
        date_iso: &str,
        availability: &str,
    ) -> Option<String> {
        self.summarize_day_outcome(system, signal, date_iso, availability)
            .text
    }

    /// [`summarize_day_with`](Self::summarize_day_with) 과 같은 일을 하되 **실패 사유까지** 돌려준다.
    ///
    /// 문서가 '요약을 안 한 날' 과 '요약이 실패한 날' 을 구분할 수 있게 하는 유일한 통로다
    /// (`docs/product-plan.md` V2). 병합(reduce)만 실패하면 세션별 압축본을 부분 요약으로 돌려준다.
    pub fn summarize_day_outcome(
        &self,
        system: &str,
        signal: &str,
        date_iso: &str,
        availability: &str,
    ) -> SummaryOutcome {
        if self.provider() == Provider::None {
            // 사용자가 끈 것(provider = none)은 실패가 아니다 — 문서도 '사용 안 함' 으로 적는다.
            if self.cfg.provider.trim() == "none" {
                tracing::info!("요약기: 사용 안 함 (수집 데이터만 정리)");
                return SummaryOutcome::default();
            }
            return SummaryOutcome::failed(NO_SUMMARIZER);
        }
        if signal.chars().count() <= self.cfg.map_reduce_chars {
            return SummaryOutcome::of(self.reduce(system, signal, date_iso, availability));
        }
        let marker = format!("\n{SESSION_SECTION_HEADER}");
        let Some((frame, sess)) = signal.split_once(&marker) else {
            // 쪼갤 세션 섹션이 없음
            return SummaryOutcome::of(self.reduce(system, signal, date_iso, availability));
        };
        let blocks: Vec<(String, String)> = sess
            .trim()
            .split("\n\n### ")
            .enumerate()
            .filter_map(|(i, chunk)| {
                let chunk = chunk.trim();
                if chunk.is_empty() {
                    return None;
                }
                let chunk = if i > 0 {
                    format!("### {chunk}")
                } else {
                    chunk.to_string()
                };
                let label = chunk
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim_start_matches(['#', ' '])
                    .trim()
                    .to_string();
                Some((label, chunk))
            })
            .collect();
        if blocks.is_empty() {
            return SummaryOutcome::of(self.reduce(system, signal, date_iso, availability));
        }

        tracing::info!(
            "신호 {}자(>{}) → map-reduce: 세션 {}개 개별 요약(병렬 {}) 후 종합",
            signal.chars().count(),
            self.cfg.map_reduce_chars,
            blocks.len(),
            self.cfg.map_workers
        );
        let (mut condensed, mut mapped_ok) = self.map_condense(&blocks, false);
        let assemble = |condensed: &[(String, String)]| {
            let sess_md = condensed
                .iter()
                .filter(|(_, s)| !s.is_empty())
                .map(|(l, s)| format!("### {l}\n{s}"))
                .collect::<Vec<_>>()
                .join("\n\n");
            format!(
                "{}\n\n## Claude Code 세션 요약\n{sess_md}",
                frame.trim_end()
            )
        };
        let mut new_signal = assemble(&condensed);
        if new_signal.chars().count() > self.cfg.map_reduce_chars {
            // 작은 세션이 많아 통과분만으로도 여전히 크면 전부 강제 압축(reduce 입력 폭주 방지).
            (condensed, mapped_ok) = self.map_condense(&blocks, true);
            new_signal = assemble(&condensed);
        }
        match self.reduce(system, &new_signal, date_iso, availability) {
            Ok(t) => SummaryOutcome::ok(t),
            // 세션별 압축은 됐는데 종합만 실패 — 빈손으로 돌아가지 말고 압축본을 그대로 붙인다.
            Err(e) if mapped_ok > 0 => {
                tracing::warn!(
                    "종합(reduce) 실패: {e} → 세션별 압축본 {mapped_ok}건으로 부분 요약"
                );
                SummaryOutcome {
                    text: Some(partial_text(&condensed)),
                    error: Some(e),
                }
            }
            Err(e) => SummaryOutcome::failed(e),
        }
    }

    /// 세션 블록들을 병렬로 개별 압축. `force_all` 이 아니면 작은 세션은 LLM 없이 원문 유지하고
    /// 큰 세션만 압축(임계 초과면 조각내 2단).
    ///
    /// 반환: ([(라벨, 본문 요약), ...] (입력 순서), **성공한 압축 호출 수**).
    /// 뒤의 수가 0 이면 map 단계가 통째로 실패한 것이라 부분 요약도 내지 않는다.
    fn map_condense(
        &self,
        blocks: &[(String, String)],
        force_all: bool,
    ) -> (Vec<(String, String)>, usize) {
        use rayon::prelude::*;

        let small = std::cmp::max(600, self.cfg.map_reduce_chars / 12); // 이보다 작은 세션은 질답 원문 그대로
        let workers = self.cfg.map_workers.clamp(1, blocks.len().max(1));
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()
            .ok();
        let total = blocks.len();
        let done = AtomicUsize::new(0);
        let ok_calls = AtomicUsize::new(0);
        self.report("요약", &format!("세션 0/{total}"));
        let condense_one = |(label, block): &(String, String)| -> (String, String) {
            let finished = |me: &Self| {
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                me.report("요약", &format!("세션 {n}/{total}"));
            };
            if !force_all && block.chars().count() <= small {
                finished(self);
                return (label.clone(), block_body(block)); // 작은 세션: 압축 없이 원문(호출 절약)
            }
            let big = if block.chars().count() > self.cfg.map_reduce_chars {
                self.condense_chunks(label, block, &ok_calls)
            } else {
                block.clone()
            };
            let summ = self.call_map(CONDENSE_SYSTEM_KO, &big);
            finished(self);
            // 폴백은 이미 만든 조각요약(big) 우선, 그것도 비면 세션 원문에서 질답을 가져온다.
            let fallback = || {
                let v = verbatim_fallback(&big);
                if v.is_empty() {
                    verbatim_fallback(block)
                } else {
                    v
                }
            };
            match summ {
                Ok(s) if plausible_summary(&s) => {
                    ok_calls.fetch_add(1, Ordering::Relaxed);
                    (label.clone(), s.trim().to_string())
                }
                // 요약이 아닌 응답(인사말·거절)은 시간 초과와 같게 — 실패로 치고 폴백한다.
                Ok(s) => {
                    tracing::warn!("세션 압축 응답이 요약이 아님({label}): {}", snippet(&s));
                    (label.clone(), fallback())
                }
                Err(_) => (label.clone(), fallback()),
            }
        };
        let out: Vec<(String, String)> = match pool {
            Some(pool) => pool.install(|| blocks.par_iter().map(condense_one).collect()),
            None => blocks.iter().map(condense_one).collect(),
        };
        (out, ok_calls.load(Ordering::Relaxed))
    }

    /// 초대형 세션 블록을 줄 단위로 조각내 각 조각을 먼저 요약, 이어붙인다(세션 내부 map).
    fn condense_chunks(&self, label: &str, block: &str, ok_calls: &AtomicUsize) -> String {
        let mut lines = block.lines();
        let header = lines.next().unwrap_or(label).to_string();
        let mut chunks: Vec<String> = Vec::new();
        let mut cur: Vec<&str> = Vec::new();
        let mut size = 0usize;
        for ln in lines {
            let n = ln.chars().count();
            if size + n > self.cfg.map_reduce_chars && !cur.is_empty() {
                chunks.push(cur.join("\n"));
                cur.clear();
                size = 0;
            }
            cur.push(ln);
            size += n + 1;
        }
        if !cur.is_empty() {
            chunks.push(cur.join("\n"));
        }
        let total = chunks.len();
        let mut parts: Vec<String> = Vec::new();
        for (i, ch) in chunks.iter().enumerate() {
            let prompt = format!("{header}\n(파트 {}/{total})\n{ch}", i + 1);
            // 조각 요약도 '요약처럼 생긴' 응답만 받는다 — 인사말·거절은 그 조각을 버린다.
            if let Ok(s) = self.call_map(CONDENSE_SYSTEM_KO, &prompt)
                && plausible_summary(&s)
            {
                ok_calls.fetch_add(1, Ordering::Relaxed);
                parts.push(s.trim().to_string());
            }
        }
        format!("{header}\n{}", parts.join("\n"))
    }
}

/// `### 헤더\n<본문>` 에서 헤더 줄을 떼고 본문만.
fn block_body(block: &str) -> String {
    block
        .split_once('\n')
        .map(|(_, b)| b)
        .unwrap_or("")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 가짜 호출기가 돌려주는 '요약처럼 생긴' 응답 — 20자 이상·한글·불릿([`plausible_summary`]).
    const FAKE_SUMMARY: &str = "- 10:00 오늘 작업 요약 한 줄입니다 (a1b2c3d)";
    /// 세션별 압축(map) 자리의 가짜 응답.
    const FAKE_CONDENSED: &str = "- 10:00 세션 압축 한 줄입니다 (요청·결정 정리)";

    /// system 프롬프트를 기록하고 요약 한 줄을 돌려주는 가짜 호출기.
    struct Fake(Mutex<Vec<String>>);
    impl LlmCaller for Fake {
        fn call(
            &self,
            system: &str,
            _user: &str,
            _cfg: &SummarizerConfig,
            _cancel: Option<&AtomicBool>,
        ) -> Option<String> {
            self.0.lock().unwrap().push(system.to_string());
            Some(FAKE_SUMMARY.into())
        }
    }

    /// 표준 템플릿의 종합(reduce) 프롬프트.
    fn standard() -> String {
        template::system_prompt("standard")
    }

    fn cfg(chars: usize, workers: usize) -> SummarizerConfig {
        SummarizerConfig {
            provider: "claude_cli".into(),
            map_reduce_chars: chars,
            map_workers: workers,
            ..Default::default()
        }
    }

    /// N7 — 빈 모델은 '제공자 기본값'. CLI 는 `--model` 생략, API 는 기본 모델 상수.
    #[test]
    fn empty_model_means_provider_default() {
        let c = SummarizerConfig::default();
        assert_eq!(c.model, ""); // 새 설정의 기본값
        assert_eq!(configured_model(&c), None);
        assert_eq!(
            configured_model(&c).unwrap_or(DEFAULT_API_MODEL),
            DEFAULT_API_MODEL
        );
        assert!(!DEFAULT_API_MODEL.is_empty());

        // 공백만 적힌 값도 비운 것으로 본다.
        let c = SummarizerConfig {
            model: "  \t".into(),
            ..Default::default()
        };
        assert_eq!(configured_model(&c), None);

        // 값이 있으면 앞뒤 공백만 떼고 그대로 쓴다(임의 치환 없음).
        let c = SummarizerConfig {
            model: " claude-sonnet-5 ".into(),
            ..Default::default()
        };
        assert_eq!(configured_model(&c), Some("claude-sonnet-5"));
    }

    #[test]
    fn provider_resolution() {
        assert_eq!(resolve_provider("none"), Provider::None);
        assert_eq!(resolve_provider("claude_cli"), Provider::ClaudeCli);
        assert_eq!(resolve_provider("anthropic_api"), Provider::AnthropicApi);
        assert_eq!(resolve_provider("claude"), Provider::None); // 오타 → 경고 후 생략
        let s = Summarizer::new(SummarizerConfig {
            provider: "none".into(),
            ..Default::default()
        });
        assert_eq!(s.summarize("# facts", "2026-07-06", ""), None);
        assert_eq!(s.summarize_day("# facts", "2026-07-06", ""), None);
    }

    #[test]
    fn single_call_when_light() {
        let fake = Box::new(Fake(Mutex::new(vec![])));
        let ptr: *const Fake = &*fake;
        let s = Summarizer::with_caller(cfg(1500, 2), fake);
        assert_eq!(
            s.summarize_day("## Git\n- 커밋", "2026-07-08", "")
                .as_deref(),
            Some(FAKE_SUMMARY)
        );
        // SAFETY: caller 는 Summarizer 가 살아있는 동안 유효.
        let calls = unsafe { &*ptr }.0.lock().unwrap().clone();
        assert_eq!(calls, vec![standard()]);
    }

    #[test]
    fn template_prompt_reaches_the_reduce_call() {
        let fake = Box::new(Fake(Mutex::new(vec![])));
        let ptr: *const Fake = &*fake;
        let s = Summarizer::with_caller(cfg(1500, 2), fake);
        let want = template::system_prompt("retro");
        assert_eq!(
            s.summarize_day_with(&want, "## Git\n- 커밋", "2026-07-08", "")
                .as_deref(),
            Some(FAKE_SUMMARY)
        );
        // SAFETY: caller 는 Summarizer 가 살아있는 동안 유효.
        let calls = unsafe { &*ptr }.0.lock().unwrap().clone();
        assert_eq!(calls, vec![want.clone()]);
        assert_ne!(want, standard());
        assert!(want.contains("막힌 것 · 배운 것"));
    }

    #[test]
    fn map_reduce_condenses_each_session_then_reduces() {
        let fake = Box::new(Fake(Mutex::new(vec![])));
        let ptr: *const Fake = &*fake;
        let s = Summarizer::with_caller(cfg(1500, 2), fake);
        let line = "- 10:00 Q: 어떤 주제 질문입니다 → A: 어떤 응답 요지입니다\n";
        let blocks: Vec<String> = ["s1", "s2", "s3"]
            .iter()
            .map(|n| format!("### [p] {n}\n{}", line.repeat(28))) // 각 ~800자(>small, <chunk)
            .collect();
        let signal = format!(
            "## Git\n- 커밋\n\n{SESSION_SECTION_HEADER}\n{}",
            blocks.join("\n\n")
        );
        assert!(signal.chars().count() > 1500);
        assert_eq!(
            s.summarize_day(&signal, "2026-07-08", "").as_deref(),
            Some(FAKE_SUMMARY)
        );
        let calls = unsafe { &*ptr }.0.lock().unwrap().clone();
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.as_str() == CONDENSE_SYSTEM_KO)
                .count(),
            3
        );
        assert_eq!(calls.last(), Some(&standard()));
    }

    #[test]
    fn oversized_session_is_chunked() {
        let fake = Box::new(Fake(Mutex::new(vec![])));
        let ptr: *const Fake = &*fake;
        let s = Summarizer::with_caller(cfg(1500, 1), fake);
        let line = "- 10:00 Q: 어떤 긴 질문입니다 → A: 어떤 긴 답변입니다\n";
        let big = format!("### [p] s1\n{}", line.repeat(200)); // ~8k자 > map_reduce_chars
        let signal = format!("## Git\n- x\n\n{SESSION_SECTION_HEADER}\n{big}");
        assert_eq!(
            s.summarize_day(&signal, "2026-07-08", "").as_deref(),
            Some(FAKE_SUMMARY)
        );
        let calls = unsafe { &*ptr }.0.lock().unwrap().clone();
        assert!(
            calls
                .iter()
                .filter(|c| c.as_str() == CONDENSE_SYSTEM_KO)
                .count()
                >= 2
        ); // 청크별 압축 여러 번
        assert_eq!(calls.last(), Some(&standard())); // 마지막은 종합
    }

    /// system 프롬프트마다 정해진 답을 주는 가짜 호출기 — 호출 때 받은 상한(초)도 기록한다.
    /// `reduce_err` 가 Some 이면 종합(reduce, 템플릿 프롬프트) 호출만 그 사유로 실패시킨다.
    struct Scripted {
        seen: Mutex<Vec<(String, u64)>>,
        reduce_err: Option<String>,
        map_err: Option<String>,
    }

    impl Scripted {
        fn new(reduce_err: Option<&str>, map_err: Option<&str>) -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
                reduce_err: reduce_err.map(str::to_string),
                map_err: map_err.map(str::to_string),
            }
        }
    }

    impl LlmCaller for Scripted {
        fn call(
            &self,
            system: &str,
            user: &str,
            cfg: &SummarizerConfig,
            cancel: Option<&AtomicBool>,
        ) -> Option<String> {
            self.call_detailed(system, user, cfg, cancel).ok()
        }

        fn call_detailed(
            &self,
            system: &str,
            _user: &str,
            cfg: &SummarizerConfig,
            _cancel: Option<&AtomicBool>,
        ) -> Result<String, String> {
            self.seen
                .lock()
                .unwrap()
                .push((system.to_string(), cfg.timeout_secs));
            let is_map = system == CONDENSE_SYSTEM_KO;
            match (is_map, &self.map_err, &self.reduce_err) {
                (true, Some(e), _) => Err(e.clone()),
                (false, _, Some(e)) => Err(e.clone()),
                (true, None, _) => Ok(FAKE_CONDENSED.into()),
                (false, _, None) => Ok(FAKE_SUMMARY.into()),
            }
        }
    }

    /// map-reduce 가 일어나는 큰 신호(세션 3개).
    fn heavy_signal() -> String {
        let line = "- 10:00 Q: 어떤 주제 질문입니다 → A: 어떤 응답 요지입니다\n";
        let blocks: Vec<String> = ["s1", "s2", "s3"]
            .iter()
            .map(|n| format!("### [p] {n}\n{}", line.repeat(28)))
            .collect();
        format!(
            "## Git\n- 커밋\n\n{SESSION_SECTION_HEADER}\n{}",
            blocks.join("\n\n")
        )
    }

    /// V2-1 — 상한은 설정값. 종합(reduce)은 전체, 세션별 압축(map)은 min(설정값, 300초).
    #[test]
    fn reduce_uses_full_timeout_and_map_is_capped() {
        let caller = Box::new(Scripted::new(None, None));
        let ptr: *const Scripted = &*caller;
        let mut c = cfg(1500, 2);
        c.timeout_secs = 600;
        let s = Summarizer::with_caller(c, caller);
        assert_eq!(
            s.summarize_day(&heavy_signal(), "2026-09-14", "")
                .as_deref(),
            Some(FAKE_SUMMARY)
        );
        // SAFETY: caller 는 Summarizer 가 살아있는 동안 유효.
        let seen = unsafe { &*ptr }.seen.lock().unwrap().clone();
        for (system, secs) in &seen {
            let want = if system == CONDENSE_SYSTEM_KO {
                300
            } else {
                600
            };
            assert_eq!(*secs, want, "{system:.20}");
        }
        assert_eq!(seen.last().map(|(_, t)| *t), Some(600)); // 마지막이 종합
        assert!(seen.iter().any(|(sys, _)| sys == CONDENSE_SYSTEM_KO));

        // 설정 상한이 map 천장보다 낮으면 map 도 그 값을 쓴다(더 키우지 않는다).
        let caller = Box::new(Scripted::new(None, None));
        let ptr: *const Scripted = &*caller;
        let mut c = cfg(1500, 2);
        c.timeout_secs = 120;
        let s = Summarizer::with_caller(c, caller);
        s.summarize_day(&heavy_signal(), "2026-09-14", "");
        let seen = unsafe { &*ptr }.seen.lock().unwrap().clone();
        assert!(seen.iter().all(|(_, t)| *t == 120));
        assert_eq!(MAP_TIMEOUT_CAP_SECS, 300);
    }

    /// V2-2 — '요약을 안 한 날' 과 '요약이 실패한 날' 을 결과에서 구분한다.
    #[test]
    fn outcome_separates_off_from_failed() {
        // (a) 사용자가 끈 것 — 실패가 아니다.
        let s = Summarizer::new(SummarizerConfig {
            provider: "none".into(),
            ..Default::default()
        });
        let o = s.summarize_day_outcome(&standard(), "# facts", "2026-09-14", "");
        assert_eq!(o, SummaryOutcome::default());
        assert!(o.text.is_none() && o.error.is_none());

        // (b) 요약기를 찾지 못함(모르는 provider) — 사유가 남는다.
        let s = Summarizer::new(SummarizerConfig {
            provider: "오타".into(),
            ..Default::default()
        });
        let o = s.summarize_day_outcome(&standard(), "# facts", "2026-09-14", "");
        assert!(o.text.is_none());
        assert_eq!(o.error.as_deref(), Some(NO_SUMMARIZER));
        assert!(NO_SUMMARIZER.starts_with("요약기 없음"));

        // (c) 정상.
        let s = Summarizer::with_caller(cfg(1500, 2), Box::new(Scripted::new(None, None)));
        let o = s.summarize_day_outcome(&standard(), "## Git\n- 커밋", "2026-09-14", "");
        assert_eq!(o, SummaryOutcome::ok(FAKE_SUMMARY));

        // (d) 단일 호출 실패 — 사유 그대로. 얇은 껍데기는 여전히 None 만 준다.
        let s = Summarizer::with_caller(
            cfg(1500, 2),
            Box::new(Scripted::new(Some("claude CLI 시간 초과(600초)"), None)),
        );
        let o = s.summarize_day_outcome(&standard(), "## Git\n- 커밋", "2026-09-14", "");
        assert_eq!(o.text, None);
        assert_eq!(o.error.as_deref(), Some("claude CLI 시간 초과(600초)"));
        assert_eq!(
            s.summarize_day_with(&standard(), "## Git\n- 커밋", "2026-09-14", ""),
            None
        );
    }

    /// V2-2 — map 은 됐는데 종합만 실패하면 세션별 압축본을 부분 요약으로 돌려준다.
    #[test]
    fn partial_summary_when_only_reduce_fails() {
        let caller = Box::new(Scripted::new(Some("claude CLI 시간 초과(600초)"), None));
        let s = Summarizer::with_caller(cfg(1500, 2), caller);
        let o = s.summarize_day_outcome(&standard(), &heavy_signal(), "2026-09-14", "");
        let text = o.text.expect("부분 요약");
        assert!(text.starts_with(&format!("> {PARTIAL_HEADING}")));
        assert!(text.contains("병합 단계 실패"));
        assert_eq!(text.matches("세션 압축").count(), 3); // 세션 3개 압축본이 그대로
        assert!(text.contains("### [p] s1") && text.contains("### [p] s3"));
        assert_eq!(o.error.as_deref(), Some("claude CLI 시간 초과(600초)"));
        // 얇은 껍데기(기존 호출자)는 부분 요약 본문을 그대로 받는다.
        assert_eq!(
            s.summarize_day_with(&standard(), &heavy_signal(), "2026-09-14", ""),
            Some(text)
        );

        // map 까지 전부 실패하면 부분 요약도 없다 — 사유만 남는다.
        let s = Summarizer::with_caller(
            cfg(1500, 2),
            Box::new(Scripted::new(
                Some("claude CLI 시간 초과(600초)"),
                Some("API 오류(HTTP 529)"),
            )),
        );
        let o = s.summarize_day_outcome(&standard(), &heavy_signal(), "2026-09-14", "");
        assert_eq!(o.text, None);
        assert_eq!(o.error.as_deref(), Some("claude CLI 시간 초과(600초)"));
    }

    /// 정해진 문자열을 map·reduce 자리에 각각 돌려주는 가짜 호출기(실패 없이 '이상한 응답'만).
    struct Canned {
        map: String,
        reduce: String,
    }

    impl Canned {
        fn new(map: &str, reduce: &str) -> Box<Self> {
            Box::new(Self {
                map: map.to_string(),
                reduce: reduce.to_string(),
            })
        }
    }

    impl LlmCaller for Canned {
        fn call(
            &self,
            system: &str,
            _user: &str,
            _cfg: &SummarizerConfig,
            _cancel: Option<&AtomicBool>,
        ) -> Option<String> {
            Some(if system == CONDENSE_SYSTEM_KO {
                self.map.clone()
            } else {
                self.reduce.clone()
            })
        }
    }

    /// 강제 실패 실험(09-15)에서 실제로 조각 요약 자리에 돌아와 문서에 붙었던 문장.
    const GREETING: &str = "I'll help you with your work. What would you like me to do?";

    /// V2 TASK E-1 — '비어 있지 않다' 는 성공이 아니다. 인사말·거절·너무 짧은 응답은 요약이 아니다.
    #[test]
    fn greetings_and_refusals_are_not_summaries() {
        for bad in [
            GREETING,
            "",
            "   \n  ",
            "요약 완료",                                       // 20자 미만
            "How can I help you today with this project?",     // 인사말
            "I can't help with that request, sorry about it.", // 거절
            "죄송합니다. 요청하신 내용은 처리할 수 없습니다.", // 거절(한글)
            "무엇을 도와드릴까요? 필요한 작업을 알려 주세요.",
            "Nothing worth reporting for this session today.", // 한글도 불릿도 없음
        ] {
            assert!(!plausible_summary(bad), "{bad:?}");
        }
        // 대소문자는 가리지 않는다.
        assert!(!plausible_summary(
            "I'LL HELP YOU with your work — tell me what you need."
        ));

        for good in [
            FAKE_SUMMARY,
            FAKE_CONDENSED,
            "- 10:00 수집기 중복 제거 진행 (a1b2c3d)\n- 11:00 회귀 테스트 추가",
            "### 세션 요약\n버그 원인을 찾아 재발 방지 테스트까지 넣었다.",
            "* fixed the collector dedup bug and added a regression test", // 한글은 없지만 불릿
            "1. 수집 경로 정리\n2. 렌더 경고 문구 추가",
        ] {
            assert!(plausible_summary(good), "{good:?}");
        }
    }

    /// V2 TASK E-1 — 인사말이 온 종합(reduce)은 실패로 적힌다(문서에 붙이지 않는다).
    #[test]
    fn greeting_answer_fails_the_day_summary() {
        assert_eq!(IMPLAUSIBLE_SUMMARY, "요약 응답이 요약이 아님(인사말/거절)");

        // (a) 가벼운 날(단일 호출) — 본문 없이 사유만.
        let s = Summarizer::with_caller(cfg(1500, 2), Canned::new(FAKE_CONDENSED, GREETING));
        let o = s.summarize_day_outcome(&standard(), "## Git\n- 커밋", "2026-09-15", "");
        assert_eq!(o.text, None);
        assert_eq!(o.error.as_deref(), Some(IMPLAUSIBLE_SUMMARY));

        // (b) 무거운 날 — 세션 압축은 됐으니 부분 요약으로 떨어지고, 인사말은 문서에 없다.
        let s = Summarizer::with_caller(cfg(1500, 2), Canned::new(FAKE_CONDENSED, GREETING));
        let o = s.summarize_day_outcome(&standard(), &heavy_signal(), "2026-09-15", "");
        let text = o.text.expect("부분 요약");
        assert!(text.starts_with(&format!("> {PARTIAL_HEADING}")));
        assert!(!text.contains("I'll help you"));
        assert_eq!(text.matches("세션 압축").count(), 3);
        assert_eq!(o.error.as_deref(), Some(IMPLAUSIBLE_SUMMARY));

        // (c) 세션 압축까지 인사말이면 성공한 압축이 0 — 부분 요약도 내지 않는다.
        let s = Summarizer::with_caller(cfg(1500, 2), Canned::new(GREETING, GREETING));
        let o = s.summarize_day_outcome(&standard(), &heavy_signal(), "2026-09-15", "");
        assert_eq!(o.text, None);
        assert_eq!(o.error.as_deref(), Some(IMPLAUSIBLE_SUMMARY));
    }

    /// 한 세션의 압축만 실패시키는 가짜 호출기 — 종합은 늘 실패(부분 요약 경로).
    struct MapFailsFor(&'static str);

    impl LlmCaller for MapFailsFor {
        fn call(
            &self,
            system: &str,
            user: &str,
            cfg: &SummarizerConfig,
            cancel: Option<&AtomicBool>,
        ) -> Option<String> {
            self.call_detailed(system, user, cfg, cancel).ok()
        }

        fn call_detailed(
            &self,
            system: &str,
            user: &str,
            _cfg: &SummarizerConfig,
            _cancel: Option<&AtomicBool>,
        ) -> Result<String, String> {
            if system != CONDENSE_SYSTEM_KO || user.contains(self.0) {
                return Err("claude CLI 시간 초과(600초)".into());
            }
            Ok(FAKE_CONDENSED.into())
        }
    }

    /// V2 TASK E-2 — 압축이 실패한 세션은 질답 원문으로 되돌리되 세션당 여덟 줄까지만.
    #[test]
    fn failed_map_pastes_at_most_eight_qa_lines() {
        assert_eq!(VERBATIM_QA_LINES, 8);

        // 순수 함수: 상한 이하면 그대로, 넘으면 여덟 줄 + 생략 한 줄.
        let few = "- 10:00 Q: 질문 하나 → A: 답 하나\n- 10:05 Q: 질문 둘 → A: 답 둘";
        assert_eq!(verbatim_fallback(&format!("### [p] s\n{few}")), few);
        let many = (0..12)
            .map(|i| format!("- Q{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cut = verbatim_fallback(&format!("### [p] s\n{many}"));
        assert_eq!(cut.lines().count(), VERBATIM_QA_LINES + 1);
        assert!(cut.starts_with("- Q0") && cut.contains("- Q7") && !cut.contains("- Q8"));
        assert!(cut.ends_with("- … 외 4개 질답 생략"));

        // 문서 경로: s2 만 압축 실패 → s1·s3 은 압축본, s2 는 질답 8줄 + 생략 줄.
        let s = Summarizer::with_caller(cfg(1500, 2), Box::new(MapFailsFor("[p] s2")));
        let o = s.summarize_day_outcome(&standard(), &heavy_signal(), "2026-09-15", "");
        let text = o.text.expect("부분 요약");
        assert_eq!(text.matches("세션 압축").count(), 2);
        assert!(text.contains("- … 외 20개 질답 생략")); // 세션당 질답 28줄 중 20줄 접힘
        let s2 = text
            .split("### [p] s2")
            .nth(1)
            .and_then(|t| t.split("### ").next())
            .expect("s2 구간");
        assert_eq!(s2.matches("Q:").count(), VERBATIM_QA_LINES);
        assert_eq!(o.error.as_deref(), Some("claude CLI 시간 초과(600초)"));
    }

    /// V2 — 압축이 실패한 세션을 질답 원문으로 되돌릴 때도 **주입 발화는 옮기지 않는다**.
    /// 블록은 `render_session_blocks` 가 만든 진짜 출력이라 렌더→폴백→문서까지 한 번에 검증된다.
    #[test]
    fn failed_map_never_pastes_injected_utterances() {
        use crate::model::{DailyData, QaTurn, Session, SessionData};
        use crate::render::{render_session_blocks, render_session_section};

        const INJECTED: &str =
            "<recommended_plugins> Here is a list of plugins the user may want to install";
        let turn = |q: &str| QaTurn {
            time: "10:00".into(),
            question: q.into(),
            answer: "어떤 응답 요지입니다".into(),
        };
        let session = |title: &str, injected: bool| {
            let mut qa: Vec<QaTurn> = Vec::new();
            if injected {
                qa.push(turn(INJECTED));
            }
            qa.extend((0..28).map(|i| turn(&format!("어떤 주제 질문입니다 {i}"))));
            Session {
                project: Some("p".into()),
                title: Some(title.into()),
                qa,
                ..Default::default()
            }
        };
        let mut d = DailyData::new(
            chrono::NaiveDate::from_ymd_opt(2026, 9, 21).unwrap(),
            "Asia/Seoul",
        );
        d.claude = Some(SessionData {
            sessions: vec![session("s1", false), session("s2", true)],
        });
        let blocks = render_session_blocks(&d, crate::time::get_tz("Asia/Seoul"), 8);
        let signal = format!("## Git\n- 커밋\n\n{}", render_session_section(&blocks));
        // 요약기에 들어가는 신호부터 이미 깨끗하다.
        assert!(!signal.contains("recommended_plugins"), "{signal:.400}");
        assert!(signal.contains("- (시스템 주입 발화 1개 생략)"));

        // s2 만 압축 실패 → 질답 원문 폴백. 주입 발화는 문서에도 나오지 않는다.
        let s = Summarizer::with_caller(cfg(1500, 2), Box::new(MapFailsFor("[p] s2")));
        let o = s.summarize_day_outcome(&standard(), &signal, "2026-09-21", "");
        let text = o.text.expect("부분 요약");
        assert!(!text.contains("recommended_plugins"), "{text}");
        assert!(text.contains("- (시스템 주입 발화 1개 생략)"), "{text}");
        let s2 = text
            .split("### [p] s2")
            .nth(1)
            .and_then(|t| t.split("### ").next())
            .expect("s2 구간");
        // 여덟 줄 상한은 그대로 — 생략 안내 한 줄 + 질답 일곱 줄.
        assert_eq!(s2.matches("Q:").count(), VERBATIM_QA_LINES - 1);
        assert_eq!(o.error.as_deref(), Some("claude CLI 시간 초과(600초)"));
    }

    /// 실패 사유는 사람이 읽을 한 줄로 다듬는다(문서·알림에 그대로 들어간다).
    #[test]
    fn failure_reasons_are_short_one_liners() {
        assert_eq!(snippet(" a \n b  c "), "a b c");
        let long = snippet(&"가".repeat(300));
        assert_eq!(long.chars().count(), 160);
        assert!(long.ends_with('…'));
        // 기본 구현(사유를 모르는 가짜 호출기)도 무언가는 남긴다.
        struct Silent;
        impl LlmCaller for Silent {
            fn call(
                &self,
                _s: &str,
                _u: &str,
                _c: &SummarizerConfig,
                _x: Option<&AtomicBool>,
            ) -> Option<String> {
                None
            }
        }
        let s = Summarizer::with_caller(cfg(1500, 2), Box::new(Silent));
        let o = s.summarize_day_outcome(&standard(), "## Git\n- 커밋", "2026-09-14", "");
        assert_eq!(o.text, None);
        assert!(o.error.is_some());
    }

    #[test]
    fn prompt_and_helpers() {
        let p = user_prompt("2026-07-08", "SIG", "");
        assert!(p.starts_with("가용 데이터: (표기 없음)\n\n아래는 2026-07-08 활동 데이터"));
        assert!(p.ends_with("---\nSIG\n---\n"));
        assert!(user_prompt("d", "s", "가용 X").starts_with("가용 X\n\n"));
        assert_eq!(block_body("### h\nbody\n"), "body");
        assert_eq!(block_body("### h"), "");
        // meta 세션 판별 서명과 일치해야 함 — 어느 템플릿으로 요약해도.
        for t in template::all() {
            assert!(template::system_prompt(t.id).starts_with("너는 하루치 개발 활동"));
        }
    }
}
