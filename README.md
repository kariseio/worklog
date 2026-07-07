# 업무일지 생성기 (worklog-generator)

> **오늘 뭐 했더라?**
>
> 하루 종일 이것저것 하다 보면 저녁엔 뭘 했는지 까먹는다. 주간보고 쓸 때, 회고할 때,
> "분명 바빴는데 왜 기억이 안 나지" 싶을 때가 많았다. 그래서 그날 실제로 한 일을
> **기억이 아니라 데이터로** 확인하려고 만들었다.
>
> 그날의 **git 커밋 · Claude Code 작업 · 회의 · 앱 사용 시간**을 자동으로 긁어모아
> **시간대별 업무일지**로 정리해 준다.

## 무엇을 하나

1. **수집** — 그날 데이터를 자동으로
   - **Git** 커밋 (여러 저장소, 변경량 포함)
   - **Claude Code** 세션 (무슨 작업을 했고 어떤 파일을 고쳤는지)
   - **NaverWorks 캘린더** 회의·일정
   - **ActivityWatch** 앱별 실사용 시간
2. **정리** — Claude 가 "몇 시엔 뭐, 몇 시엔 뭐"를 **시간대별**로 요약하고,
   결정론적 **지표·타임라인**(커밋 수·집중 시간·핵심 성과·커밋 타입 등)을 덧붙인다
3. **저장** — **Markdown(문서 폴더)** · **Obsidian** · **Notion** 중 원하는 곳에

각 소스는 독립적으로 켜고 끌 수 있고, 준비 안 된 건 자동으로 건너뛴다. (git·Claude 만으로도 바로 동작)

---

# 사용법

## 1. 가장 쉬운 방법 — 실행파일(.exe)

`releases\worklog.exe` (항상 최신 버전) 를 **더블클릭**. 파이썬 설치 없이 앱 창이 뜬다. 다른 PC 로 복사해도 실행된다.

- 날짜 선택 → **↻ 생성** → 요약·지표·타임라인 확인 → **저장**
- 저장 위치: **문서(Documents)\업무일지\YYYY-MM-DD.md** (현재 로그인 사용자 폴더 자동 감지)
- 창을 닫으면 **시스템 트레이**로 들어간다 — 트레이 메뉴: `열기` / `오늘 업무일지 생성` / `종료`
- 이미 실행 중이면 새로 안 뜨고 **기존 창이 앞으로** 온다 (단일 인스턴스)
- 전부 로컬(`127.0.0.1`)에서만 동작, 외부 전송 없음

## 2. 연동 설정 (Obsidian / Notion / NaverWorks)

앱 우측 상단 **⚙ 설정** 에서 입력하고 **[연결 테스트]** 로 즉시 확인한다.

| 연동 | 넣는 값 | 효과 |
|---|---|---|
| **Obsidian** | vault 경로 · 하위 폴더 | 그 vault 에 일지 저장 |
| **Notion** | 통합 토큰 · 대상 페이지/DB ID | 그 아래 페이지로 저장 (통합을 대상에 **연결(Connections)** 필수) |
| **NaverWorks** | Client ID/Secret · Service Account · Private Key · 사용자 ID | 회의가 시간대별 업무·타임라인에 📅 로 들어감 |

- **NaverWorks 준비**: [developers.worksmobile.com](https://developers.worksmobile.com) 에서 앱 등록 → 서비스 계정·private key 발급 → 관리자에서 **`calendar.read`** 스코프 승인. 캘린더는 설정에서 **[불러오기]** 로 목록을 받아 **여러 개 체크** 가능.
- 입력값은 이 PC의 `~/.worklog/settings.json` 에만 저장(외부 전송 없음). 토큰·시크릿은 조회 시 `설정됨` 으로만 표시되고, 빈 칸으로 저장하면 기존 값이 유지된다.
- **AI 요약**은 실행 PC 에 `claude` CLI 가 있으면 자동 사용(별도 키 불필요). 없으면 `ANTHROPIC_API_KEY` 를 쓰거나 요약 없이 수집·지표만.

## 3. 명령줄 (CLI)

파이썬 환경이라면 명령으로도 쓸 수 있다.

```bash
worklog                        # 오늘 업무일지 생성 → 문서 폴더에 저장
worklog --yesterday            # 어제
worklog --date 2026-07-05      # 특정 날짜
worklog --dry-run              # 저장하지 않고 콘솔 미리보기
worklog --no-llm               # AI 요약 없이 데이터만
worklog --sources git,claude   # 특정 소스만
worklog --app                  # 데스크톱 앱 실행 ( = worklog-app )
worklog --init                 # config.yaml / .env 템플릿 생성
```

- CLI 에서 감시할 git 저장소는 `config.yaml` 의 `sources.git.repos` / `scan_roots` 에 지정한다. (`include_claude_cwds: true` 면 그날 Claude 로 작업한 폴더도 자동 포함)
- 비밀 값은 `.env` 에 둔다(`worklog --init` 이 템플릿 생성). **git 에 커밋 금지.**

## 4. 매일 자동 생성 (선택)

Windows 작업 스케줄러에 다음을 등록하면 매일 알아서 만들어진다.

```
worklog --yesterday
```

---

## 개발 / 빌드

```bash
uv venv
uv pip install -e ".[app]"     # 앱까지 (fastapi · uvicorn · pywebview · pystray)
uv run pytest -q               # 테스트

build_exe.bat                  # 단일 exe 빌드 → releases\worklog-<버전>.exe 자동 보관
```

파이프라인: **수집기 4개 → `render`(사실 정리) → `analyze`(지표·타임라인) → `summarize`(Claude 시간대별 요약) → 출력(md/Obsidian/Notion)**.
CLI 와 데스크톱 앱은 둘 다 `service.py` 하나를 호출하므로 로직이 한 벌이다.
