//! 설정 — `~/.worklog/settings.json` 하나로 통합(v2).
//!
//! v1(Python)이 쓰던 파일 레이아웃을 키 이름 그대로 읽는다. 모르는 키(예: 옛 `activitywatch`)는
//! 무시되고 다음 저장 때 사라진다. `config.yaml`/`.env` 는 더 이상 읽지 않는다.
//!
//! 비밀 값(NaverWorks client_secret/private_key, Notion token)도 이 파일에 있다(로컬 전용).
//! UI 가 빈 칸으로 저장해도 기존 비밀 값이 지워지지 않도록 [`Config::merge_blank_from`] 을 쓴다.
//!
//! 컷오버 전까지는 v1 exe 와 같은 파일을 공유하므로, v1 이 그대로 못 읽는 값(빈 `markdown.dir`)은
//! 저장하지 않는다.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use chrono::Weekday;
use serde::{Deserialize, Deserializer, Serialize};

use crate::paths;

// --------------------------------------------------------------------------- //
// 수집 소스
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GitConfig {
    pub enabled: bool,
    /// 직접 지정한 저장소 절대경로.
    pub repos: Vec<String>,
    /// 자동 탐색 루트. `scan_all_drives` 면 무시.
    pub scan_roots: Vec<String>,
    /// true 면 모든 고정 디스크를 탐색.
    pub scan_all_drives: bool,
    /// 루트 아래 몇 단계까지 내려가며 저장소를 찾을지 (1~12).
    #[serde(deserialize_with = "lenient_depth")]
    pub scan_depth: u32,
    /// 비우면 '내 커밋만' 자동 필터. 값이 있으면 그대로 `--author` 패턴.
    pub author: String,
    /// 추가 '내 신원'(이메일/핸들) 목록 — 자동감지와 OR.
    pub authors: Vec<String>,
    /// Claude/Codex 세션의 작업 폴더를 git 대상에 자동 포함.
    pub include_claude_cwds: bool,
}

pub const SCAN_DEPTH_DEFAULT: u32 = 5;
pub const SCAN_DEPTH_MIN: u32 = 1;
pub const SCAN_DEPTH_MAX: u32 = 12;

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            repos: Vec::new(),
            scan_roots: Vec::new(),
            scan_all_drives: true,
            scan_depth: SCAN_DEPTH_DEFAULT,
            author: String::new(),
            authors: Vec::new(),
            include_claude_cwds: true,
        }
    }
}

/// 숫자·숫자문자열은 받고, 그 외(예: "bad")는 기본값 5 로. (v1 서버의 관용 파싱과 동일)
fn lenient_depth<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    let n = match &v {
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        serde_json::Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    };
    Ok(
        n.map(|n| n.clamp(SCAN_DEPTH_MIN as i64, SCAN_DEPTH_MAX as i64) as u32)
            .unwrap_or(SCAN_DEPTH_DEFAULT),
    )
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeConfig {
    pub enabled: bool,
    /// 비우면 `paths::claude_projects_dir()`.
    pub projects_dir: String,
    pub include_read: bool,
    pub max_intent_len: usize,
    /// 세션당 수집할 질답 상한(초과분은 앞부분 생략).
    pub max_qa_turns: usize,
    /// 질답의 '답' 요지 최대 길이.
    pub max_answer_len: usize,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            projects_dir: String::new(),
            include_read: false,
            max_intent_len: 300,
            max_qa_turns: 120,
            max_answer_len: 180,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexConfig {
    pub enabled: bool,
    /// 비우면 `paths::codex_sessions_dir()`.
    pub sessions_dir: String,
    pub include_read: bool,
    pub max_intent_len: usize,
    pub max_qa_turns: usize,
    pub max_answer_len: usize,
    /// 초대형 롤아웃 파일 스트리밍 상한(행 수).
    pub max_lines: usize,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sessions_dir: String::new(),
            include_read: false,
            max_intent_len: 300,
            max_qa_turns: 120,
            max_answer_len: 180,
            max_lines: 200_000,
        }
    }
}

/// 설정 UI 표시용 캘린더 이름.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CalendarInfo {
    pub calendar_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NaverWorksConfig {
    pub enabled: bool,
    /// 캘린더 소유자 member ID(이메일).
    pub user_id: String,
    /// (구버전 단일) 하위호환용.
    pub calendar_id: String,
    /// 다중 선택.
    pub calendar_ids: Vec<String>,
    pub calendars: Vec<CalendarInfo>,
    pub scope: String,
    pub client_id: String,
    pub client_secret: String,
    pub service_account: String,
    /// PEM 내용.
    pub private_key: String,
    pub private_key_path: String,
}

impl Default for NaverWorksConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            user_id: String::new(),
            calendar_id: String::new(),
            calendar_ids: Vec::new(),
            calendars: Vec::new(),
            scope: "calendar.read".into(),
            client_id: String::new(),
            client_secret: String::new(),
            service_account: String::new(),
            private_key: String::new(),
            private_key_path: String::new(),
        }
    }
}

impl NaverWorksConfig {
    /// 조회할 캘린더 ID 목록. 다중 선택 우선, 없으면 단일, 그것도 없으면 빈 목록(=기본 캘린더).
    pub fn effective_calendar_ids(&self) -> Vec<String> {
        if !self.calendar_ids.is_empty() {
            return self.calendar_ids.clone();
        }
        if !self.calendar_id.trim().is_empty() {
            return vec![self.calendar_id.clone()];
        }
        Vec::new()
    }

    pub fn has_private_key(&self) -> bool {
        !self.private_key.trim().is_empty() || !self.private_key_path.trim().is_empty()
    }

    /// 수집에 필요한데 비어 있는 항목 이름들.
    pub fn missing_credentials(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.client_id.trim().is_empty() {
            out.push("client_id");
        }
        if self.client_secret.trim().is_empty() {
            out.push("client_secret");
        }
        if self.service_account.trim().is_empty() {
            out.push("service_account");
        }
        if self.user_id.trim().is_empty() {
            out.push("user_id");
        }
        if !self.has_private_key() {
            out.push("private_key");
        }
        out
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourcesConfig {
    pub git: GitConfig,
    pub claude: ClaudeConfig,
    pub codex: CodexConfig,
    pub naverworks: NaverWorksConfig,
    /// 요약 프롬프트에서 뺄 저장소·폴더 글롭 (예: `D:\works\a-corp\**`).
    ///
    /// 수집은 그대로 하되, 여기에 걸린 세션·커밋은 LLM 에 보내는 신호에서 빠지고
    /// 문서에는 [`crate::exclude::PRIVATE_PROJECT`] 집계 한 행으로만 남는다.
    /// (product-plan §3 D7 · §4 원칙 4 · §5-1 N0)
    pub exclude: Vec<String>,
}

// --------------------------------------------------------------------------- //
// 요약기 · 출력
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SummarizerConfig {
    /// auto | claude_cli | anthropic_api | none
    pub provider: String,
    /// 요약에 쓸 모델 이름. **빈 문자열이 기본값** = 제공자 기본 모델에 맡긴다
    /// (claude CLI 는 `--model` 을 붙이지 않고, Anthropic API 는
    /// [`crate::summarize::DEFAULT_API_MODEL`] 을 쓴다).
    /// 이미 값이 있는 설정 파일은 [`Config::normalize`] 가 건드리지 않는다(마이그레이션 없음).
    pub model: String,
    pub language: String,
    pub max_tokens: u32,
    /// 세션 질답 총량이 이 글자수를 넘으면 map-reduce.
    pub map_reduce_chars: usize,
    /// 세션별 요약 병렬 수.
    pub map_workers: usize,
    /// 기본 일지 템플릿 — standard | report | retro ([`crate::template::TEMPLATE_IDS`]).
    /// 모르는 값은 [`Config::normalize`] 에서 `standard` 로 돌아간다.
    pub template: String,
}

impl Default for SummarizerConfig {
    fn default() -> Self {
        Self {
            provider: "auto".into(),
            // 빈 값 = 제공자 기본 모델. 특정 모델을 강제하지 않는다(UI placeholder "기본 모델").
            model: String::new(),
            language: "ko".into(),
            max_tokens: 4000,
            map_reduce_chars: 20_000,
            map_workers: 4,
            template: crate::template::DEFAULT_TEMPLATE.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MarkdownOutputConfig {
    pub enabled: bool,
    /// 비우면 문서\업무일지. 빈 값은 파일에 쓰지 않는다(v1 은 빈 dir 를 그대로 경로로 써 버린다).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub dir: String,
}

impl Default for MarkdownOutputConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: String::new(),
        }
    }
}

impl MarkdownOutputConfig {
    pub fn resolved_dir(&self) -> PathBuf {
        paths::dir_or(&self.dir, paths::default_markdown_dir)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ObsidianOutputConfig {
    pub enabled: bool,
    pub vault_dir: String,
    pub subdir: String,
}

impl Default for ObsidianOutputConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            vault_dir: String::new(),
            subdir: "업무일지".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotionOutputConfig {
    pub enabled: bool,
    /// page | database
    pub parent_type: String,
    pub parent_id: String,
    pub title_prop: String,
    /// Notion-Version 헤더.
    pub version: String,
    pub token: String,
}

impl Default for NotionOutputConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            parent_type: "page".into(),
            parent_id: String::new(),
            title_prop: "Name".into(),
            version: "2022-06-28".into(),
            token: String::new(),
        }
    }
}

impl NotionOutputConfig {
    pub fn is_configured(&self) -> bool {
        self.enabled && !self.token.trim().is_empty() && !self.parent_id.trim().is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputsConfig {
    pub markdown: MarkdownOutputConfig,
    pub obsidian: ObsidianOutputConfig,
    pub notion: NotionOutputConfig,
}

// --------------------------------------------------------------------------- //
// 자동화 (v2 신규) — 정해진 시각 동작 · 실시간 수집 · 시작 · 알림 · 단축키
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleMode {
    /// "오늘 일지 만들까요?" 알림만. 생성은 사용자가 직접 누른다. (기본)
    #[default]
    Notify,
    /// 그 시각에 일지를 만들고 저장한다(AI 호출 발생).
    Generate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduleConfig {
    pub enabled: bool,
    /// "HH:MM" (로컬 시간대).
    pub time: String,
    /// ISO 요일 번호 1=월 … 7=일.
    pub weekdays: Vec<u8>,
    pub mode: ScheduleMode,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            time: "18:30".into(),
            weekdays: vec![1, 2, 3, 4, 5],
            mode: ScheduleMode::Notify,
        }
    }
}

impl ScheduleConfig {
    /// "HH:MM" → (시, 분). 형식이 나쁘면 None.
    pub fn time_hm(&self) -> Option<(u32, u32)> {
        let (h, m) = self.time.trim().split_once(':')?;
        let h: u32 = h.parse().ok()?;
        let m: u32 = m.parse().ok()?;
        (h < 24 && m < 60).then_some((h, m))
    }

    pub fn on_weekday(&self, wd: Weekday) -> bool {
        self.weekdays.contains(&(wd.number_from_monday() as u8))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RealtimeConfig {
    /// Claude/Codex 세션·git 커밋을 파일 감시로 즉시 반영.
    pub enabled: bool,
    /// NaverWorks 회의 확인 주기(분). 푸시가 없어 폴링.
    pub meeting_poll_min: u32,
    /// 놓친 이벤트 보정용 전체 재수집 주기(분).
    pub full_rescan_min: u32,
}

impl Default for RealtimeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            meeting_poll_min: 15,
            full_rescan_min: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutomationConfig {
    pub schedule: ScheduleConfig,
    pub realtime: RealtimeConfig,
    /// Windows 시작 시 자동 실행(트레이).
    pub autostart: bool,
    /// 완료·실패·시각 알림을 Windows 알림으로.
    pub notify: bool,
    /// 빠른 메모 전역 단축키. 비우면 사용 안 함.
    pub global_shortcut: String,
}

impl Default for AutomationConfig {
    fn default() -> Self {
        Self {
            schedule: ScheduleConfig::default(),
            realtime: RealtimeConfig::default(),
            autostart: false,
            notify: true,
            global_shortcut: "Ctrl+Shift+Space".into(),
        }
    }
}

// --------------------------------------------------------------------------- //
// 겉모양 (v2 신규) — 테마 · 글꼴 · 글자 크기
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceConfig {
    /// system | light | dark
    pub theme: String,
    /// rounded(둥근 고딕 NanumSquareRound + 제목 Jua) | sketch(Gaegu 손글씨) | plain(기본 고딕)
    pub font: String,
    /// small | normal | large
    pub text_size: String,
}

/// 고를 수 있는 값들. 그 외·빈 값은 [`Config::normalize`] 에서 기본값으로 되돌린다.
pub const THEMES: [&str; 3] = ["system", "light", "dark"];
pub const FONTS: [&str; 3] = ["rounded", "sketch", "plain"];
pub const TEXT_SIZES: [&str; 3] = ["small", "normal", "large"];

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            font: "rounded".into(),
            text_size: "normal".into(),
        }
    }
}

// --------------------------------------------------------------------------- //
// 최상위
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub timezone: String,
    /// 저장 파일 하단에 '수집 데이터 원본' 부록 포함 여부.
    pub include_raw_data: bool,
    pub summarizer: SummarizerConfig,
    pub outputs: OutputsConfig,
    pub sources: SourcesConfig,
    pub automation: AutomationConfig,
    /// 화면 겉모양(테마·글꼴·글자 크기). 옛 파일에는 없으므로 없으면 기본값.
    pub appearance: AppearanceConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timezone: "Asia/Seoul".into(),
            include_raw_data: false,
            summarizer: SummarizerConfig::default(),
            outputs: OutputsConfig::default(),
            sources: SourcesConfig::default(),
            automation: AutomationConfig::default(),
            appearance: AppearanceConfig::default(),
        }
    }
}

/// 비밀 값 존재 여부(UI 에 원값 대신 '설정됨' 표시용).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretsPresence {
    pub naverworks_client_secret: bool,
    pub naverworks_private_key: bool,
    pub notion_token: bool,
}

/// 설정 파일을 읽은 결과의 출처. `ReadFailed` 면 저장을 막아 실제 파일을 덮어쓰지 않아야 한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadStatus {
    /// 파일을 정상적으로 읽었다.
    Loaded,
    /// 파일이 없어 기본값.
    Missing,
    /// 내용이 손상돼(JSON/UTF-8 오류) `.json.bak` 으로 옮기고 기본값.
    CorruptedBackedUp(String),
    /// 읽기 자체가 실패(권한·잠금 등). 기본값이지만 파일은 그대로 있다.
    ReadFailed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub config: Config,
    pub status: LoadStatus,
}

impl Loaded {
    /// 저장해도 사용자의 실제 설정을 잃지 않는 상태인지.
    pub fn safe_to_save(&self) -> bool {
        !matches!(self.status, LoadStatus::ReadFailed(_))
    }
}

/// 수집 소스 이름(고정 순서).
pub const SOURCE_NAMES: [&str; 4] = ["git", "claude", "codex", "naverworks"];

impl Config {
    /// 기본 경로(`paths::settings_path()`)에서 읽는다. 없으면 기본값.
    pub fn load() -> Self {
        Self::load_from(&paths::settings_path())
    }

    pub fn load_with_status() -> Loaded {
        Self::load_from_with_status(&paths::settings_path())
    }

    /// 파일에서 읽는다. 없으면 기본값. 손상됐으면 `.json.bak` 으로 보존하고 기본값.
    pub fn load_from(path: &Path) -> Self {
        Self::load_from_with_status(path).config
    }

    /// [`load_from`](Self::load_from) 과 같되 출처를 함께 돌려준다.
    pub fn load_from_with_status(path: &Path) -> Loaded {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Loaded {
                    config: Self::default(),
                    status: LoadStatus::Missing,
                };
            }
            Err(e) => {
                tracing::warn!(
                    "설정 파일 읽기 실패({}): {e} → 기본값(저장 금지)",
                    path.display()
                );
                return Loaded {
                    config: Self::default(),
                    status: LoadStatus::ReadFailed(e.to_string()),
                };
            }
        };
        // UTF-8 BOM(편집기가 붙이기도 함)은 무시.
        let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
        let parsed = std::str::from_utf8(body)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<Config>(t).map_err(|e| e.to_string()));
        match parsed {
            Ok(mut c) => {
                c.normalize();
                Loaded {
                    config: c,
                    status: LoadStatus::Loaded,
                }
            }
            Err(e) => {
                tracing::warn!(
                    "설정 파일 손상({}): {e} → .bak 보존 후 기본값",
                    path.display()
                );
                let bak = path.with_extension("json.bak");
                if let Err(e2) = fs::rename(path, &bak) {
                    tracing::warn!("손상 파일 보존 실패: {e2}");
                }
                Loaded {
                    config: Self::default(),
                    status: LoadStatus::CorruptedBackedUp(e),
                }
            }
        }
    }

    pub fn save(&self) -> io::Result<PathBuf> {
        let p = paths::settings_path();
        self.save_to(&p)?;
        Ok(p)
    }

    /// 임시 파일에 다 쓴 뒤 교체 — 중간에 죽어도 기존 파일이 반쯤 쓰인 채 남지 않는다.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// 값 정리: 깊이 클램프, 빈 문자열·공백 항목 제거, 중복 제거.
    pub fn normalize(&mut self) {
        let g = &mut self.sources.git;
        g.scan_depth = g.scan_depth.clamp(SCAN_DEPTH_MIN, SCAN_DEPTH_MAX);
        g.authors = dedupe_trimmed(&g.authors);
        g.scan_roots = dedupe_trimmed(&g.scan_roots);
        g.repos = dedupe_trimmed(&g.repos);
        self.sources.exclude = dedupe_trimmed(&self.sources.exclude);
        let nw = &mut self.sources.naverworks;
        nw.calendar_ids = dedupe_trimmed(&nw.calendar_ids);
        if nw.scope.trim().is_empty() {
            nw.scope = "calendar.read".into();
        }
        let s = &mut self.automation.schedule;
        s.weekdays.retain(|d| (1..=7).contains(d));
        s.weekdays.sort_unstable();
        s.weekdays.dedup();
        if s.time_hm().is_none() {
            s.time = "18:30".into();
        }
        if self.timezone.trim().is_empty() {
            self.timezone = "Asia/Seoul".into();
        }
        // `summarizer.model` 은 일부러 손대지 않는다 — 빈 값은 '제공자 기본 모델' 이라는 뜻이고,
        // 기존 파일에 적힌 모델 이름은 마이그레이션 없이 그대로 존중한다.
        // 일지 템플릿도 정해진 값만 — 모르는 값·빈 칸은 standard 로.
        self.summarizer.template = crate::template::resolve(&self.summarizer.template).to_string();
        // 겉모양은 정해진 값만 — 모르는 값·빈 칸은 기본값으로.
        let d = AppearanceConfig::default();
        let a = &mut self.appearance;
        keep_known(&mut a.theme, &THEMES, &d.theme);
        keep_known(&mut a.font, &FONTS, &d.font);
        keep_known(&mut a.text_size, &TEXT_SIZES, &d.text_size);
    }

    /// UI 저장 정책(v1 과 동일): 비밀·헤더 값이 빈 칸이면 기존 값을 유지한다.
    /// 대상 — NaverWorks `scope`·`client_secret`·`private_key`, Notion `version`·`token`.
    /// 그 외(사용자 ID, client_id, 서비스 계정, 키 경로, parent_id …)는 빈 값으로 지울 수 있다.
    pub fn merge_blank_from(&mut self, old: &Config) {
        let (n, o) = (&mut self.sources.naverworks, &old.sources.naverworks);
        keep_if_blank(&mut n.scope, &o.scope);
        keep_if_blank(&mut n.client_secret, &o.client_secret);
        keep_if_blank(&mut n.private_key, &o.private_key);
        let (n, o) = (&mut self.outputs.notion, &old.outputs.notion);
        keep_if_blank(&mut n.version, &o.version);
        keep_if_blank(&mut n.token, &o.token);
    }

    pub fn secrets_presence(&self) -> SecretsPresence {
        SecretsPresence {
            naverworks_client_secret: !self.sources.naverworks.client_secret.trim().is_empty(),
            naverworks_private_key: !self.sources.naverworks.private_key.trim().is_empty(),
            notion_token: !self.outputs.notion.token.trim().is_empty(),
        }
    }

    /// 비밀 값을 지운 사본(UI 로 내보낼 때).
    pub fn redacted(&self) -> Config {
        let mut c = self.clone();
        c.sources.naverworks.client_secret.clear();
        c.sources.naverworks.private_key.clear();
        c.outputs.notion.token.clear();
        c
    }

    /// 켜진 수집 소스 이름(고정 순서).
    pub fn enabled_sources(&self) -> Vec<&'static str> {
        let s = &self.sources;
        [
            ("git", s.git.enabled),
            ("claude", s.claude.enabled),
            ("codex", s.codex.enabled),
            ("naverworks", s.naverworks.enabled),
        ]
        .into_iter()
        .filter_map(|(n, on)| on.then_some(n))
        .collect()
    }
}

/// 앞뒤 공백을 떼고 소문자로 맞춘 값이 `allowed` 안에 있으면 그 값으로, 아니면 `fallback` 으로.
fn keep_known(target: &mut String, allowed: &[&str], fallback: &str) {
    let v = target.trim().to_lowercase();
    *target = if allowed.contains(&v.as_str()) {
        v
    } else {
        fallback.to_string()
    };
}

fn keep_if_blank(target: &mut String, old: &str) {
    if target.trim().is_empty() && !old.is_empty() {
        *target = old.to_string();
    }
}

fn dedupe_trimmed(items: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for it in items {
        let t = it.trim();
        if !t.is_empty() && !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v1(Python) 앱이 실제로 남긴 settings.json 과 같은 모양(옛 activitywatch 키 포함).
    const V1_SAMPLE: &str = r#"{
      "timezone": "Asia/Seoul",
      "summarizer": {"provider": "auto", "model": "claude-opus-4-8"},
      "outputs": {
        "obsidian": {"enabled": true, "vault_dir": "C:\\Users\\me\\Documents\\Obsidian Vault", "subdir": "업무일지"},
        "notion": {"enabled": false, "parent_type": "page", "parent_id": "", "title_prop": "Name"},
        "markdown": {"enabled": true, "dir": "C:\\Users\\me\\Documents\\업무일지"}
      },
      "sources": {
        "naverworks": {"enabled": true, "user_id": "me@x.kr", "calendar_id": "c_old",
                       "client_id": "cid", "service_account": "sa@x.kr", "private_key_path": "./k.key",
                       "client_secret": "s3cr3t", "scope": "calendar.read",
                       "calendar_ids": ["c_new"], "calendars": [{"calendar_id": "c_new", "name": "내 캘린더"}]},
        "git": {"enabled": true, "scan_all_drives": false, "scan_roots": ["D:\\"], "scan_depth": 5,
                "author": "", "include_claude_cwds": true},
        "activitywatch": {"enabled": true, "base_url": "http://localhost:5600"},
        "claude": {"enabled": true}
      },
      "include_raw_data": false
    }"#;

    #[test]
    fn reads_v1_layout_and_ignores_legacy_keys() {
        let c: Config = serde_json::from_str(V1_SAMPLE).unwrap();
        assert_eq!(c.timezone, "Asia/Seoul");
        assert!(c.outputs.obsidian.enabled);
        assert_eq!(c.outputs.obsidian.subdir, "업무일지");
        assert_eq!(c.outputs.markdown.dir, "C:\\Users\\me\\Documents\\업무일지");
        assert_eq!(c.sources.naverworks.calendar_ids, vec!["c_new"]);
        assert_eq!(c.sources.naverworks.effective_calendar_ids(), vec!["c_new"]);
        assert_eq!(c.sources.naverworks.calendars[0].name, "내 캘린더");
        assert!(!c.sources.git.scan_all_drives);
        assert_eq!(c.sources.git.scan_roots, vec!["D:\\"]);
        // v2 신규 섹션은 기본값으로 채워진다.
        assert_eq!(c.automation.schedule.mode, ScheduleMode::Notify);
        assert_eq!(c.automation.schedule.time, "18:30");
        // codex 는 v1 파일에 없어도 기본 켜짐.
        assert!(c.sources.codex.enabled);
        // 저장하면 옛 키는 사라진다.
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("activitywatch"));
    }

    #[test]
    fn legacy_single_calendar_fallback() {
        let nw = NaverWorksConfig {
            calendar_id: "c_old".into(),
            ..Default::default()
        };
        assert_eq!(nw.effective_calendar_ids(), vec!["c_old"]);
        assert!(
            NaverWorksConfig::default()
                .effective_calendar_ids()
                .is_empty()
        );
    }

    #[test]
    fn missing_file_gives_default_and_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("settings.json");
        let l = Config::load_from_with_status(&p);
        assert_eq!(l.config, Config::default());
        assert_eq!(l.status, LoadStatus::Missing);
        assert!(l.safe_to_save());

        let mut c2 = Config::default();
        c2.outputs.notion.token = "ntn_secret".into();
        c2.sources.git.scan_depth = 7;
        c2.save_to(&p).unwrap();
        assert!(!p.with_extension("json.tmp").exists());
        let l = Config::load_from_with_status(&p);
        assert_eq!(l.config, c2);
        assert_eq!(l.status, LoadStatus::Loaded);
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(raw["outputs"]["notion"]["token"], "ntn_secret");
        // 빈 markdown.dir 는 파일에 쓰지 않는다(v1 exe 가 "" 를 경로로 써 버리지 않게).
        assert!(raw["outputs"]["markdown"].get("dir").is_none());
        assert_eq!(raw["outputs"]["markdown"]["enabled"], true);

        // 기존 파일 덮어쓰기(운영 경로)도 원자적으로 된다.
        c2.timezone = "UTC".into();
        c2.outputs.markdown.dir = "D:/x".into();
        c2.save_to(&p).unwrap();
        let back = Config::load_from(&p);
        assert_eq!(back.timezone, "UTC");
        assert_eq!(back.outputs.markdown.dir, "D:/x");
    }

    #[test]
    fn corrupted_file_is_backed_up_but_read_failure_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        fs::write(&p, "{ not json").unwrap();
        let l = Config::load_from_with_status(&p);
        assert_eq!(l.config, Config::default());
        assert!(matches!(l.status, LoadStatus::CorruptedBackedUp(_)));
        assert!(!p.exists());
        assert!(dir.path().join("settings.json.bak").exists());

        // 잘못된 UTF-8(CP949 로 다시 저장된 파일) 도 손상으로 본다.
        fs::write(&p, b"{\"timezone\": \"\xBE\xF7\xB9\xAB\"}").unwrap();
        let l = Config::load_from_with_status(&p);
        assert!(matches!(l.status, LoadStatus::CorruptedBackedUp(_)));
        assert!(!p.exists());

        // BOM 은 무시된다.
        fs::write(&p, b"\xEF\xBB\xBF{\"timezone\": \"UTC\"}").unwrap();
        let l = Config::load_from_with_status(&p);
        assert_eq!(l.status, LoadStatus::Loaded);
        assert_eq!(l.config.timezone, "UTC");

        // 읽기 실패(경로가 디렉터리)는 기본값이되 파일을 건드리지 않고 저장 금지.
        let d = dir.path().join("dir.json");
        fs::create_dir_all(&d).unwrap();
        let l = Config::load_from_with_status(&d);
        assert!(matches!(l.status, LoadStatus::ReadFailed(_)));
        assert!(!l.safe_to_save());
        assert!(d.is_dir());
    }

    #[test]
    fn load_from_normalizes_file_values() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        fs::write(
            &p,
            r#"{"timezone":" ","automation":{"schedule":{"weekdays":[9,1,1],"time":"25:00"}},
                "sources":{"git":{"authors":[" a ","a"]}}}"#,
        )
        .unwrap();
        let c = Config::load_from(&p);
        assert_eq!(c.timezone, "Asia/Seoul");
        assert_eq!(c.automation.schedule.weekdays, vec![1]);
        assert_eq!(c.automation.schedule.time, "18:30");
        assert_eq!(c.sources.git.authors, vec!["a"]);
    }

    #[test]
    fn depth_is_lenient_and_clamped() {
        let c: Config = serde_json::from_str(r#"{"sources":{"git":{"scan_depth":999}}}"#).unwrap();
        assert_eq!(c.sources.git.scan_depth, 12);
        let c: Config = serde_json::from_str(r#"{"sources":{"git":{"scan_depth":0}}}"#).unwrap();
        assert_eq!(c.sources.git.scan_depth, 1);
        let c: Config =
            serde_json::from_str(r#"{"sources":{"git":{"scan_depth":"bad"}}}"#).unwrap();
        assert_eq!(c.sources.git.scan_depth, 5);
        let c: Config = serde_json::from_str(r#"{"sources":{"git":{"scan_depth":"7"}}}"#).unwrap();
        assert_eq!(c.sources.git.scan_depth, 7);
    }

    #[test]
    fn blank_secret_keeps_existing_but_nonsecret_updates() {
        let mut old = Config::default();
        old.outputs.notion.token = "ntn_A".into();
        old.outputs.notion.parent_id = "p1".into();
        old.outputs.notion.version = "2022-06-28".into();
        old.sources.naverworks.client_id = "cid".into();
        old.sources.naverworks.service_account = "sa@dom".into();
        old.sources.naverworks.client_secret = "sec".into();
        old.sources.naverworks.scope = "calendar".into();

        let mut new = Config::default();
        new.outputs.notion.enabled = true;
        new.outputs.notion.parent_id = "p2".into();
        new.outputs.notion.token = String::new();
        new.outputs.notion.version = String::new();
        new.sources.naverworks.enabled = true;
        new.sources.naverworks.client_id = String::new();
        new.sources.naverworks.service_account = String::new();
        new.sources.naverworks.scope = String::new();
        new.merge_blank_from(&old);

        // 비밀·헤더 값은 유지
        assert_eq!(new.outputs.notion.token, "ntn_A");
        assert_eq!(new.outputs.notion.version, "2022-06-28");
        assert_eq!(new.sources.naverworks.client_secret, "sec");
        assert_eq!(new.sources.naverworks.scope, "calendar");
        // 비밀이 아닌 값은 그대로(빈 값으로 지울 수 있음) — v1 과 동일
        assert_eq!(new.outputs.notion.parent_id, "p2");
        assert_eq!(new.sources.naverworks.client_id, "");
        assert_eq!(new.sources.naverworks.service_account, "");
        assert!(new.sources.naverworks.enabled);
    }

    #[test]
    fn redaction_and_presence() {
        let mut c = Config::default();
        c.outputs.notion.token = "t".into();
        c.sources.naverworks.client_secret = "s".into();
        let p = c.secrets_presence();
        assert!(p.notion_token && p.naverworks_client_secret && !p.naverworks_private_key);
        let r = c.redacted();
        assert!(r.outputs.notion.token.is_empty());
        assert!(r.sources.naverworks.client_secret.is_empty());
        assert_eq!(c.outputs.notion.token, "t"); // 원본은 그대로
    }

    #[test]
    fn normalize_and_helpers() {
        let mut c = Config::default();
        c.sources.git.authors = vec![" a@x ".into(), "".into(), "a@x".into(), "b".into()];
        c.automation.schedule.weekdays = vec![5, 9, 1, 1, 0];
        c.automation.schedule.time = "25:99".into();
        c.timezone = " ".into();
        c.normalize();
        assert_eq!(c.sources.git.authors, vec!["a@x", "b"]);
        assert_eq!(c.automation.schedule.weekdays, vec![1, 5]);
        assert_eq!(c.automation.schedule.time, "18:30");
        assert_eq!(c.timezone, "Asia/Seoul");
        assert_eq!(c.automation.schedule.time_hm(), Some((18, 30)));
        assert!(c.automation.schedule.on_weekday(Weekday::Mon));
        assert!(!c.automation.schedule.on_weekday(Weekday::Tue));

        c.sources.codex.enabled = false;
        assert_eq!(c.enabled_sources(), vec!["git", "claude"]);
        let mut nw = NaverWorksConfig::default();
        assert_eq!(nw.missing_credentials().len(), 5);
        nw.private_key_path = "k".into();
        assert!(!nw.missing_credentials().contains(&"private_key"));
    }

    #[test]
    fn summarizer_template_defaults_and_normalizes() {
        // 옛 파일(키 없음) → 기본 템플릿.
        let c: Config = serde_json::from_str(V1_SAMPLE).unwrap();
        assert_eq!(c.summarizer.template, "standard");
        assert_eq!(
            SummarizerConfig::default().template,
            crate::template::DEFAULT_TEMPLATE
        );

        // 모르는 값·빈 칸은 standard, 공백·대문자만 다른 값은 살린다.
        let mut c = Config::default();
        c.summarizer.template = "  Retro ".into();
        c.normalize();
        assert_eq!(c.summarizer.template, "retro");
        c.summarizer.template = "없는템플릿".into();
        c.normalize();
        assert_eq!(c.summarizer.template, "standard");
        c.summarizer.template = String::new();
        c.normalize();
        assert_eq!(c.summarizer.template, "standard");

        // 파일에서 읽을 때도 같은 정리가 걸리고, 왕복 저장에 살아남는다.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        fs::write(&p, r#"{"summarizer":{"template":"REPORT"}}"#).unwrap();
        let c = Config::load_from(&p);
        assert_eq!(c.summarizer.template, "report");
        c.save_to(&p).unwrap();
        assert_eq!(Config::load_from(&p).summarizer.template, "report");
        // 비밀이 아니므로 UI 로 나가는 사본에도 남는다.
        assert_eq!(c.redacted().summarizer.template, "report");
    }

    /// N7 — 새 설정의 모델은 비어 있고(제공자 기본값), 기존 파일의 모델 값은 그대로 유지된다.
    #[test]
    fn summarizer_model_defaults_to_empty() {
        assert_eq!(SummarizerConfig::default().model, "");
        assert_eq!(Config::default().summarizer.model, "");
        // normalize 가 빈 값을 어떤 모델로도 채우지 않는다(UI 는 placeholder '기본 모델' 로 보여 준다).
        let mut c = Config::default();
        c.normalize();
        assert_eq!(c.summarizer.model, "");
        // 키가 통째로 없는 파일도 빈 값.
        let c: Config = serde_json::from_str(r#"{"summarizer":{"provider":"auto"}}"#).unwrap();
        assert_eq!(c.summarizer.model, "");
        // 저장하면 빈 문자열로 남는다(키를 지우지 않는다).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        Config::default().save_to(&p).unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(raw["summarizer"]["model"], "");
    }

    #[test]
    fn summarizer_model_keeps_existing_value() {
        // 옛 파일에 적힌 모델은 마이그레이션 없이 그대로.
        let mut c: Config = serde_json::from_str(V1_SAMPLE).unwrap();
        assert_eq!(c.summarizer.model, "claude-opus-4-8");
        c.normalize();
        assert_eq!(c.summarizer.model, "claude-opus-4-8");

        // 파일에서 읽고 다시 저장해도 값이 살아남는다.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        fs::write(&p, r#"{"summarizer":{"model":"claude-sonnet-5"}}"#).unwrap();
        let c = Config::load_from(&p);
        assert_eq!(c.summarizer.model, "claude-sonnet-5");
        c.save_to(&p).unwrap();
        assert_eq!(Config::load_from(&p).summarizer.model, "claude-sonnet-5");
        // 비밀이 아니므로 UI 로 나가는 사본에도 남는다.
        assert_eq!(c.redacted().summarizer.model, "claude-sonnet-5");
        // 사용자가 비우면 비운 대로 — '기본 모델' 로 되돌아간다.
        fs::write(&p, r#"{"summarizer":{"model":"  "}}"#).unwrap();
        assert_eq!(Config::load_from(&p).summarizer.model, "  ");
    }

    #[test]
    fn appearance_defaults() {
        let a = Config::default().appearance;
        assert_eq!(a, AppearanceConfig::default());
        assert_eq!(a.theme, "system");
        assert_eq!(a.font, "rounded"); // 둥근 고딕(NanumSquareRound + Jua) 이 기본
        assert_eq!(a.text_size, "normal");
        // 기본값은 언제나 고를 수 있는 값 안에 있어야 한다.
        assert!(THEMES.contains(&a.theme.as_str()));
        assert!(FONTS.contains(&a.font.as_str()));
        assert!(TEXT_SIZES.contains(&a.text_size.as_str()));
    }

    #[test]
    fn normalize_fixes_bad_appearance() {
        // 모르는 값·빈 칸은 기본값으로.
        let mut c = Config {
            appearance: AppearanceConfig {
                theme: "midnight".into(),
                font: "  ".into(),
                text_size: String::new(),
            },
            ..Default::default()
        };
        c.normalize();
        assert_eq!(c.appearance, AppearanceConfig::default());

        // 공백·대문자만 다른 정상 값은 다듬어서 살린다.
        let mut c = Config {
            appearance: AppearanceConfig {
                theme: " Dark ".into(),
                font: "PLAIN".into(),
                text_size: "\tLarge\n".into(),
            },
            ..Default::default()
        };
        c.normalize();
        assert_eq!(c.appearance.theme, "dark");
        assert_eq!(c.appearance.font, "plain");
        assert_eq!(c.appearance.text_size, "large");

        // 고를 수 있는 글꼴은 세 개 모두 그대로 살아남는다.
        for f in FONTS {
            let mut c = Config {
                appearance: AppearanceConfig {
                    font: f.into(),
                    ..Default::default()
                },
                ..Default::default()
            };
            c.normalize();
            assert_eq!(c.appearance.font, f);
        }

        // 파일에서 읽을 때도 같은 정리가 걸린다.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        fs::write(
            &p,
            r#"{"appearance":{"theme":"","font":"comic","text_size":" SMALL "}}"#,
        )
        .unwrap();
        let c = Config::load_from(&p);
        assert_eq!(c.appearance.theme, "system");
        assert_eq!(c.appearance.font, "rounded");
        assert_eq!(c.appearance.text_size, "small");
    }

    #[test]
    fn appearance_survives_roundtrip() {
        let c = Config {
            appearance: AppearanceConfig {
                theme: "dark".into(),
                font: "plain".into(),
                text_size: "large".into(),
            },
            ..Default::default()
        };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), c);

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        c.save_to(&p).unwrap();
        assert_eq!(Config::load_from(&p), c);
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(raw["appearance"]["theme"], "dark");
        assert_eq!(raw["appearance"]["text_size"], "large");
        // 비밀이 아니므로 UI 로 나가는 사본에도 그대로 남는다.
        assert_eq!(c.redacted().appearance, c.appearance);
    }

    #[test]
    fn appearance_missing_in_file_gives_defaults() {
        // 키가 통째로 없는 옛 settings.json.
        let c: Config = serde_json::from_str(V1_SAMPLE).unwrap();
        assert_eq!(c.appearance, AppearanceConfig::default());

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        fs::write(&p, r#"{"timezone":"Asia/Seoul"}"#).unwrap();
        assert_eq!(
            Config::load_from(&p).appearance,
            AppearanceConfig::default()
        );

        // 일부 키만 있는 파일은 나머지만 기본값.
        let c: Config = serde_json::from_str(r#"{"appearance":{"theme":"light"}}"#).unwrap();
        assert_eq!(c.appearance.theme, "light");
        assert_eq!(c.appearance.font, "rounded");
        assert_eq!(c.appearance.text_size, "normal");
    }

    #[test]
    fn markdown_dir_default_when_blank() {
        let m = MarkdownOutputConfig::default();
        assert!(m.resolved_dir().ends_with("업무일지"));
        let m = MarkdownOutputConfig {
            enabled: true,
            dir: "D:/x".into(),
        };
        assert_eq!(m.resolved_dir(), PathBuf::from("D:/x"));
    }
}
