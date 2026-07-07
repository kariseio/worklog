# 업무일지 생성기 (worklog-generator)

하루 동안의 활동을 자동으로 모아 **업무일지**를 만들어 준다.

수집 소스:
- **ActivityWatch** — 앱/창별 실사용 시간 (자리비움 제외)
- **Git** — 그날의 커밋 (여러 저장소, 변경량 포함)
- **Claude Code 세션 로그** — 무슨 작업을 요청했고 어떤 파일을 고쳤는지 (`~/.claude/projects`)
- **NaverWorks 캘린더** — 그날의 일정/회의 (서비스 계정 OAuth)

이 데이터를 **Claude 로 종합**해 자연어 업무일지를 만들고,
**Markdown 파일 / Obsidian / Notion** 중 원하는 곳에 저장한다.

각 소스는 독립적으로 켜고 끌 수 있고, 준비 안 된 소스는 자동으로 건너뛴다.
(예: ActivityWatch 미설치, NaverWorks 자격증명 미설정이어도 git+claude 만으로 동작)

---

## 설치

Python 3.10+ 필요. [uv](https://docs.astral.sh/uv/) 권장.

```bash
# 프로젝트 폴더에서
uv venv
uv pip install -e .            # 기본 (CLI, claude CLI 로 요약)
uv pip install -e ".[app]"     # 데스크톱 앱(GUI) 까지 (fastapi/uvicorn/pywebview)
uv pip install -e ".[llm]"     # Anthropic API 로 요약하고 싶을 때(anthropic SDK 추가)
```

pip 만 쓴다면:

```bash
python -m venv .venv && .venv\Scripts\activate   # Windows
pip install -e .
```

---

## 빠른 시작

```bash
worklog --init        # config.yaml / .env 템플릿 생성
# → config.yaml 에서 감시할 git 저장소 등을 설정, .env 에 비밀 값 입력

worklog               # 오늘 업무일지 생성 → ./worklogs/YYYY-MM-DD.md
worklog --yesterday   # 어제
worklog --date 2026-07-05
worklog --dry-run     # 저장하지 않고 콘솔에 미리보기
worklog --no-llm      # LLM 요약 없이 데이터만 정리
worklog --sources git,claude   # 특정 소스만
```

`python -m worklog` 로도 실행 가능.

---

## 데스크톱 앱으로 실행 (GUI)

명령어 대신 **아이콘/클릭으로 쓰고 싶으면** 앱 모드를 쓴다. 겉은 독립 창, 속은 로컬 웹(전부 `127.0.0.1`, 외부 전송 없음).

```bash
uv pip install -e ".[app]"     # fastapi + uvicorn + pywebview 추가
worklog --app                  # 또는:  worklog-app
```

- **자체 앱 창**(pywebview)이 뜬다. pywebview 가 없으면 기본 브라우저로 자동 폴백.
- 화면 흐름: 날짜 선택 → **생성** → 요약 **미리보기·편집** → **로컬 md / Obsidian / Notion** 저장. 왼쪽에 지난 일지 히스토리.
- 각 소스 상태(Git 8커밋 / Claude 13세션 / ActivityWatch 미연결 …)를 칩으로 보여주고, "근거 데이터"로 원본 커밋까지 확인 가능.

### 앱 내부 설정 (⚙ 설정)

config.yaml/.env 를 손대지 않고 **앱 안에서** 연동을 설정할 수 있다. 우측 상단 **⚙ 설정**:

- **일반**: 시간대, 요약 방식(provider)·모델
- **Obsidian**: vault 경로 / 하위 폴더 → `[연결 테스트]` 로 쓰기 가능 여부 즉시 확인
- **Notion**: 통합 토큰 / 대상 유형(page·database) / 대상 ID → `[연결 테스트]` 로 실제 Notion API 접근(토큰·공유) 확인
- **NaverWorks**: Client ID/Secret · Service Account · Private Key 경로 · 사용자 ID → `[연결 테스트]` 로 실제 토큰 발급 시도

저장하면 **`~/.worklog/settings.json`** 에만 기록되고(외부 전송 없음), config.yaml/.env 위에 오버레이되어 CLI 에도 반영된다.
토큰·시크릿은 조회 시 값이 노출되지 않고 `설정됨` 으로만 표시되며, 저장 시 **새로 입력한 경우에만** 갱신된다(빈 칸이면 기존 값 유지).

바탕화면 바로가기를 만들려면 `worklog-app` 실행 파일(`.venv\Scripts\worklog-app.exe`)의 바로가기를 만들면 된다.

### 단일 실행파일(.exe) 로 배포

파이썬/venv 없이 **더블클릭 한 번으로** 실행되는 단일 exe 로 묶을 수 있다 (PyInstaller).

```bat
uv pip install -e ".[app]" pyinstaller
build_exe.bat        REM  또는 아래 명령
```

결과물 `dist\worklog.exe` (약 23MB) 를 더블클릭하면 앱 창이 뜬다. 다른 PC 로 복사해도 실행된다.

- **요약(AI)** 만은 exe 에 포함되지 않는다 — 실행 PC 에 `claude` CLI 가 있으면 그대로 요약되고, 없으면 `ANTHROPIC_API_KEY`(설정의 anthropic_api) 를 쓰거나 요약 없이 동작한다. 수집·지표·타임라인·저장은 exe 만으로 모두 된다.
- **시스템 트레이**: 창을 닫으면 종료되지 않고 **시스템 트레이로** 들어간다. 트레이 아이콘 메뉴 — `열기` / `오늘 업무일지 생성` / `종료`(더블클릭 = 열기). 완전히 끄려면 트레이 메뉴의 `종료`. (pystray/pillow 없으면 닫기 = 종료로 동작)
- Windows 11 은 WebView2 런타임이 기본 탑재라 창이 바로 뜬다. 없으면 자동으로 브라우저로 폴백.
- 설정은 `~/.worklog/settings.json`, 저장은 **문서 폴더의 `업무일지`** 하위에 동일하게 동작한다.

---

## 설정

- `config.yaml` — 비밀이 아닌 설정 (시간대, 감시 저장소, 출력 대상 등). `config.example.yaml` 참고.
- `.env` — 비밀 값 (토큰/키). `.env.example` 참고. **git 에 커밋하지 말 것.**

### 요약기 (summarizer.provider)

| 값 | 동작 | 필요 조건 |
|---|---|---|
| `auto` (기본) | `claude` CLI 있으면 사용, 없으면 Anthropic API, 둘 다 없으면 요약 생략 | – |
| `claude_cli` | 설치된 `claude` CLI 로 요약 | Claude Code 로그인 (API 키 불필요) |
| `anthropic_api` | Anthropic SDK 로 요약 | `ANTHROPIC_API_KEY` (또는 `ant auth login`), `[llm]` extra |
| `none` | LLM 요약 없이 수집 데이터만 | – |

기본 모델은 `claude-opus-4-8`. `summarizer.model` 로 변경 가능.

**요약 동작:**
- 생성 시 각 소스(Git·Claude·NaverWorks·ActivityWatch)와 출력(로컬·Obsidian·Notion)의 **유무를 감지**해, 일지 상단에 가용 상태를 표기하고 **있는 데이터의 섹션만** 만든다(없는 소스는 아예 생략).
- Claude Code 로그는 원본 프롬프트·명령어·중복 세션을 걷어내고 **커밋 제목 + 세션 제목** 중심의 '정제 신호'로 압축한 뒤 요약한다. 요약은 과정 서술을 배제하고 **완료된 결과("무엇을 했는가")** 만 개조식으로 남긴다. (요약기 자신이 만든 세션은 피드백 루프 소음이라 자동 제외)

**시간대별 문서화:** 요약은 `## 🕘 시간대별 업무` 섹션을 먼저 만든다 — 세션·커밋·회의를 시간순으로 엮어 자연스러운 시간 블록(오전/오후 등)으로 "몇 시경 무엇을 했다"를 문서화한다. 그 아래 `## 오늘 한 일`(프로젝트별)이 따라온다. **NaverWorks 캘린더를 연동하면 회의가 그 시간축에 📅 로 끼워져** "이 시간에 무슨 회의"까지 함께 기록된다.

**지표·타임라인 (LLM 무관, 결정론적):** LLM 요약 아래에 `analyze.py` 가 만든 사실 지표 섹션이 붙는다 — ⭐ 핵심 성과(Top 3), 📊 오늘 지표(커밋·변경량·세션·토큰·활동시간·커밋 타입 분포·작업 성격), 프로젝트별 집중시간 표(세션 `first_ts~last_ts` 기반), 🕐 타임라인(세션 구간 + 커밋 시각, 타입 뱃지). 앱에서는 KPI 카드·표·타임라인으로 렌더된다. (변경량은 노력과 비례하지 않을 수 있어 캡션으로 한계 명시)

### 출력 대상

`outputs.markdown` / `outputs.obsidian` / `outputs.notion` 을 각각 켤 수 있고 **동시에** 내보낼 수 있다.
- **markdown**: `YYYY-MM-DD.md` 저장. 기본 위치는 **문서(Documents) 폴더의 `업무일지` 하위**(`outputs.markdown.dir` 로 변경 가능).
- **obsidian**: `vault_dir/subdir/YYYY-MM-DD.md` 로 저장(YAML frontmatter 포함).
- **notion**: `parent_id` 하위에 새 페이지 생성. 통합을 대상 페이지/DB 에 **연결(Connections)** 필수.

---

## 준비물별 안내

### ActivityWatch
[activitywatch.net](https://activitywatch.net) 설치 후 실행만 하면 됨.
로컬 `http://localhost:5600` REST API 를 읽는다(인증 없음). 미실행이면 자동 건너뜀.

### Git
`sources.git.repos` 에 저장소 절대경로를 나열하거나, `scan_roots` 로 폴더를 자동 탐색.
`include_claude_cwds: true` 면 Claude Code 로그에서 그날 작업한 프로젝트 경로도 자동으로 대상에 추가.

### Claude Code 로그
별도 설정 없이 `~/.claude/projects` 를 읽는다. 각 세션의 실제 작업 디렉토리(`cwd`),
요청 프롬프트, 수정한 파일, 실행 명령, 도구 사용량을 추출.

### NaverWorks 캘린더 (서비스 계정)
[NaverWorks Developers](https://developers.worksmobile.com) 에서:
1. 앱 등록 → **Client ID / Client Secret** 발급
2. **서비스 계정** 생성 → **private key** 다운로드
3. 관리자에서 **calendar.read** 스코프 승인

`.env` 에 채운다:
```
NAVERWORKS_CLIENT_ID=...
NAVERWORKS_CLIENT_SECRET=...
NAVERWORKS_SERVICE_ACCOUNT=xxxx@yourdomain
NAVERWORKS_PRIVATE_KEY_PATH=./secrets/private.key
NAVERWORKS_USER_ID=you@yourdomain     # 캘린더 소유자
```
그리고 `config.yaml` 의 `sources.naverworks.enabled: true`.

인증은 JWT-bearer(RS256) → `auth.worksmobile.com/oauth2/v2.0/token` →
`worksapis.com/v1.0/users/{userId}/calendar/events` 흐름을 사용한다.

---

## 구조

```
worklog/
├── cli.py              # 명령행 진입점
├── service.py          # 오케스트레이션 (CLI ↔ 앱 공유): 수집·요약·조합·저장·히스토리
├── config.py           # config.yaml + .env 로딩
├── models.py           # 수집 데이터/산출물 dataclass (수집기 ↔ 렌더 계약)
├── util.py             # 시간대/날짜/포맷/로깅
├── render.py           # 수집 데이터 → 결정론적 사실 Markdown
├── summarize.py        # Claude 종합 요약 (claude CLI / Anthropic API)
├── collectors/
│   ├── base.py         # Collector / CollectContext / CollectorResult
│   ├── activitywatch.py
│   ├── git_repos.py
│   ├── claude_logs.py
│   └── naverworks.py
├── outputs/
│   ├── base.py         # Sink / SinkResult
│   ├── markdown.py
│   ├── obsidian.py
│   └── notion.py
└── webapp/             # 데스크톱 앱 (선택)
    ├── server.py       # 로컬 FastAPI (UI 에 JSON 제공)
    ├── launcher.py     # uvicorn + pywebview 창
    └── static/index.html
```

파이프라인: **수집기 4개 → DailyData → render(사실) → summarize(요약) → 출력 sink**.
CLI 와 데스크톱 앱은 둘 다 `service.py` 를 호출하므로 로직이 한 벌이다.

---

## 테스트

```bash
uv pip install pytest
uv run pytest -q
```

---

## 자동 실행 (선택)

매일 저녁 자동 생성하려면 Windows 작업 스케줄러에 등록:
```
worklog --yesterday
```
(또는 이 프로젝트에서 Claude Code 의 `/schedule` 사용)
