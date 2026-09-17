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
    config::SummarizerConfig,
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

/// claude CLI 한 번 호출 상한.
const CLI_TIMEOUT: Duration = Duration::from_secs(240);
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
}

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
        match resolve_provider(&cfg.provider) {
            Provider::ClaudeCli => call_claude_cli(system, user, cfg, cancel),
            Provider::AnthropicApi => call_anthropic_api(system, user, cfg),
            Provider::None => None,
        }
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
) -> Option<String> {
    let Some(exe) = claude_exe() else {
        tracing::warn!("claude CLI 를 찾을 수 없어 요약을 건너뜁니다.");
        return None;
    };
    // 이 요약 호출이 만드는 Claude 세션을 나중에 확실히 걸러내기 위한 표식(맨 앞).
    let full = format!("{WORKLOG_SENTINEL}\n{system}\n\n{user}");
    let mut cmd = Command::new(exe);
    cmd.args(["-p", "--model", &cfg.model]);
    match run_with_timeout(cmd, &full, CLI_TIMEOUT, cancel) {
        Ok((0, out, _)) => {
            let out = out.trim();
            (!out.is_empty()).then(|| out.to_string())
        }
        Ok((code, _, err)) => {
            tracing::warn!(
                "claude CLI 요약 실패(exit {code}): {}",
                err.chars().take(300).collect::<String>()
            );
            None
        }
        Err(e) => {
            tracing::warn!("claude CLI 요약 실패: {e}");
            None
        }
    }
}

fn call_anthropic_api(system: &str, user: &str, cfg: &SummarizerConfig) -> Option<String> {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|k| !k.is_empty());
    let Some(key) = key else {
        tracing::warn!("ANTHROPIC_API_KEY 가 없어 Anthropic API 요약을 건너뜁니다.");
        return None;
    };
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .ok()?;
    let body = serde_json::json!({
        "model": cfg.model,
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
        Err(e) => {
            tracing::warn!("Anthropic API 요약 실패: {e}");
            return None;
        }
    };
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        tracing::warn!(
            "Anthropic API 요약 실패(HTTP {}): {}",
            status.as_u16(),
            text.chars().take(300).collect::<String>()
        );
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
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
    (!out.is_empty()).then(|| out.to_string())
}

// --------------------------------------------------------------------------- //
// 요약기
// --------------------------------------------------------------------------- //

/// 진행 상황 콜백: ("단계", "세부"). 예: ("요약", "세션 3/7").
pub type ProgressFn = dyn Fn(&str, &str) + Send + Sync;

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

    fn call(&self, system: &str, user: &str) -> Option<String> {
        if self.cancelled() {
            return None;
        }
        self.caller
            .call(system, user, &self.cfg, self.cancel.as_deref())
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
        self.report("요약", "종합");
        self.call(system, &user_prompt(date_iso, signal, availability))
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
        if self.provider() == Provider::None {
            tracing::info!("요약기: 사용 안 함 (수집 데이터만 정리)");
            return None;
        }
        if signal.chars().count() <= self.cfg.map_reduce_chars {
            return self.summarize_with(system, signal, date_iso, availability);
        }
        let marker = format!("\n{SESSION_SECTION_HEADER}");
        let Some((frame, sess)) = signal.split_once(&marker) else {
            // 쪼갤 세션 섹션이 없음
            return self.summarize_with(system, signal, date_iso, availability);
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
            return self.summarize_with(system, signal, date_iso, availability);
        }

        tracing::info!(
            "신호 {}자(>{}) → map-reduce: 세션 {}개 개별 요약(병렬 {}) 후 종합",
            signal.chars().count(),
            self.cfg.map_reduce_chars,
            blocks.len(),
            self.cfg.map_workers
        );
        let mut condensed = self.map_condense(&blocks, false);
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
            condensed = self.map_condense(&blocks, true);
            new_signal = assemble(&condensed);
        }
        self.summarize_with(system, &new_signal, date_iso, availability)
    }

    /// 세션 블록들을 병렬로 개별 압축. `force_all` 이 아니면 작은 세션은 LLM 없이 원문 유지하고
    /// 큰 세션만 압축(임계 초과면 조각내 2단). 반환: [(라벨, 본문 요약), ...] (입력 순서).
    fn map_condense(&self, blocks: &[(String, String)], force_all: bool) -> Vec<(String, String)> {
        use rayon::prelude::*;

        let small = std::cmp::max(600, self.cfg.map_reduce_chars / 12); // 이보다 작은 세션은 질답 원문 그대로
        let workers = self.cfg.map_workers.clamp(1, blocks.len().max(1));
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()
            .ok();
        let total = blocks.len();
        let done = AtomicUsize::new(0);
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
                self.condense_chunks(label, block)
            } else {
                block.clone()
            };
            let summ = self.call(CONDENSE_SYSTEM_KO, &big);
            finished(self);
            // 최종 압축 실패 시엔 (원문 block 이 아니라) 이미 만든 조각요약 big 으로 폴백.
            (
                label.clone(),
                summ.unwrap_or_else(|| block_body(&big)).trim().to_string(),
            )
        };
        match pool {
            Some(pool) => pool.install(|| blocks.par_iter().map(condense_one).collect()),
            None => blocks.iter().map(condense_one).collect(),
        }
    }

    /// 초대형 세션 블록을 줄 단위로 조각내 각 조각을 먼저 요약, 이어붙인다(세션 내부 map).
    fn condense_chunks(&self, label: &str, block: &str) -> String {
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
            if let Some(s) = self.call(CONDENSE_SYSTEM_KO, &prompt) {
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

    /// system 프롬프트를 기록하고 "요약" 을 돌려주는 가짜 호출기.
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
            Some("요약".into())
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
            Some("요약")
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
            Some("요약")
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
            Some("요약")
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
            Some("요약")
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
