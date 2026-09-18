# 업무일지 (worklog)

하루 동안의 행동을 저절로 모아 두었다가, 원할 때 한 번에 업무일지로 정리해 주는 Windows 데스크톱 앱.

> **오늘 뭐 했더라?**
>
> 하루 종일 이것저것 하다 보면 저녁엔 뭘 했는지 까먹는다. 주간보고 쓸 때, 회고할 때,
> "분명 바빴는데 왜 기억이 안 나지" 싶을 때가 많았다. 그래서 그날 실제로 한 일을
> **기억이 아니라 데이터로** 확인하려고 만들었다.

## 무엇을 하나

1. **실시간 수집** — 앱이 떠 있는 동안 그날 행동이 저절로 쌓인다.
   - **git 커밋** (여러 저장소, 변경량 포함)
   - **Claude Code · Codex 세션** (무슨 작업을 했고 어떤 파일을 고쳤는지)
   - **NaverWorks 회의** (캘린더 폴링)
   - **메모** — 생각날 때 한 줄씩
2. **오늘 화면 타임라인** — 모인 것들이 시간 순 피드로 보인다. 커밋 · AI 세션 · 회의 개수도 함께.
3. **‘지금 일지 만들기’** — 누를 때만 AI 가 하루를 요약해 문서를 만든다. 수집은 자동, 문서는 수동.
4. **자동 저장** — 만든 문서는 설정한 곳(Markdown 폴더 · Obsidian · Notion)에 바로 저장된다.

그 밖에:

- **정해진 시각 알림/자동 생성** — 기본은 평일 18:30 **알림만**. 설정에서 **자동 생성**으로 바꿀 수 있다.
- **트레이 상주** — 창을 닫아도 트레이에 남는다. 트레이 메뉴: `열기` / `빠른 메모` / `오늘 업무일지 생성` / `종료`.
- **빠른 메모** — 전역 단축키 **`Ctrl+Shift+Space`** (설정에서 변경·해제 가능). 어느 창에서든 한 줄 남긴다.
- 전부 로컬에서 동작한다. 요약을 쓸 때만 Claude 로 나간다.

---

## 설치

1. [GitHub Releases](https://github.com/kariseio/worklog/releases) 에서 **`Worklog_<버전>_x64-setup.exe`** 를 받아 실행한다.
2. Windows 10/11 (x64). **WebView2** 가 필요한데 요즘 Windows 에는 보통 이미 깔려 있다.
3. 한 번 설치하면 **앱 안에서 자동 업데이트**된다. (설정 → 정보 → `업데이트 확인`)

> 코드 서명을 하지 않아 SmartScreen 경고가 뜰 수 있다. **`추가 정보` → `실행`** 으로 넘어가면 된다.

---

## 화면 안내

### 오늘

- 위쪽에 **커밋 · AI 세션 · 회의** 개수, 아래에 그날의 **타임라인**.
- 맨 아래 **메모 입력창** — `Enter` 저장, `Shift+Enter` 줄바꿈. `#태그` · `@이름` 을 알아본다.
- **`지금 일지 만들기`** 로 그날 문서를 만든다.
- 날짜 이동: 헤더의 **`‹` `›`** (또는 `Alt+←` / `Alt+→`), 달력 버튼으로 특정 날짜. **지난 날짜는 그날 저장해 둔 기록**을 보여 준다(원본 세션 로그가 지워졌어도 남아 있다). `다시 읽기` 로 원본을 한 번 더 훑을 수도 있다.

### 일지

- 왼쪽 **달력**에 날짜별 상태 점: **●** 생성됨 · **◐** 편집됨 · **○** 미생성. 검색과 목록도 여기에.
- 오른쪽에 문서 — **보기 / 편집(`Ctrl+S` 저장) / 다시 생성 / 복사**, 그리고 **`그날 활동 보기`** 로 오늘 화면의 그 날짜로 건너뛴다.
- 편집한 일지는 `다시 생성` 할 때 확인을 한 번 거친다.

### 설정

탭 여섯 개: **자동화** (정해진 시각 · 실시간 수집 · 자동 시작 · 알림 · 단축키) · **모양** (테마 · 글꼴 · 글자 크기) · **수집 소스** (git · Claude · Codex · NaverWorks · 제외 목록) · **저장 대상** (Markdown · Obsidian · Notion) · **AI 요약** · **정보** (버전 · 파일 위치 · 업데이트).

---

## 데이터가 있는 곳

| 경로 | 내용 |
|---|---|
| `~/.worklog/settings.json` | 모든 설정. **비밀값(NaverWorks client secret · private key, Notion 토큰)이 들어 있으니 공유 금지.** |
| `~/.worklog/worklog.db` | SQLite — 메모 · 생성한 문서 · 실행 이력 · 저장소 캐시 · 일별 피드 스냅샷 |
| `문서\업무일지\YYYY-MM-DD.md` | 기본 Markdown 저장 폴더 (설정에서 변경 가능) |

---

## AI 요약

- **claude CLI** 가 PATH 에 있으면 그걸 쓴다. **로그인만 되어 있으면 API 키가 따로 필요 없다.**
- CLI 가 없으면 환경변수 **`ANTHROPIC_API_KEY`** 로 Anthropic API 를 쓴다.
- 설정에서 **`사용 안 함`** 을 고르면 요약 없이 수집한 데이터와 지표·타임라인만 정리해 준다.
- 요약이 실패하면(시간 초과 등) 문서 머리에 `⚠ AI 요약 시도가 실패했습니다: …` 로 사유를 적고, 병합 단계만 실패했으면 세션별 압축본을 부분 요약으로 남긴다. 긴 날은 `settings.json` 의 `summarizer.timeout_secs`(기본 600, 30~3600)를 늘린다.
- **무엇이 나가나**: 요약을 만들 때 세션 질답 · 커밋 메시지 · 회의 제목 · 메모가 Claude 로 전송된다. 그 외에는 아무것도 나가지 않는다.
- **보내고 싶지 않은 저장소**는 설정 › 수집 소스 › **제외 목록**에 글롭으로 적는다(예: `D:\works\a-corp\**`). 걸린 저장소의 세션·커밋은 요약 프롬프트에서 통째로 빠지고, 문서에는 `[비공개 프로젝트]` 한 행의 집계(커밋·세션·집중시간)만 남는다. worktree 도 본체 저장소로 알아본다.

## 연동 (설정 화면에서 값 입력 → `연결 확인`)

| 연동 | 넣는 값 |
|---|---|
| **NaverWorks** | Client ID · Client Secret · Service Account · Private Key(PEM 또는 파일 경로) · 사용자 ID(이메일). 캘린더는 `불러오기` 로 목록을 받아 여러 개 고를 수 있다. [developers.worksmobile.com](https://developers.worksmobile.com) 에서 앱을 등록하고 **`calendar.read`** 스코프를 승인받아야 한다. |
| **Notion** | 통합 토큰 · 대상 종류(페이지/데이터베이스) · 대상 ID · 제목 속성 이름. 통합을 대상 페이지에 **연결(Connections)** 해 두어야 한다. |
| **Obsidian** | vault 폴더 경로 · 그 안의 하위 폴더 |

---

## CLI (개발자용)

`worklog` 바이너리(`crates/worklog-cli`)로 앱 없이도 쓸 수 있다.

```bash
cargo run -p worklog-cli -- --date 2026-09-14 --dry-run --no-llm   # 저장·요약 없이 콘솔 미리보기
worklog note "#요청 @김팀장 결제 API 타임아웃 늘려달라"              # 메모 한 줄
worklog notes                                                      # 그날 메모 목록
worklog weekly                                                     # 이번 주(월~오늘) 주간 초안을 콘솔로
worklog weekly --from 2026-09-14 --to 2026-09-18 --out 주간.md     # 기간 지정 · 파일로
```

앱과 같은 `~/.worklog/settings.json` · `worklog.db` 를 본다. `--yesterday`, `--sources git,claude`, `--tz` 등도 있다.

**주간 초안**(`worklog weekly`)은 이미 만들어 둔 일별 일지에서 `## 오늘의 성과` 와 `## 결정 · 요청 · 할 일` 만 뽑아 프로젝트별로 모으고, 마지막 날까지 체크되지 않은 `- [ ]` 만 '남은 할 일'로 이월한다. 중복 문장 병합에만 LLM 을 한 번 쓴다(`--no-llm` 이면 생략). 어디에도 저장하지 않으며, 일지가 없는 평일과 섹션을 못 찾은 날은 경고로 표시한다.

## 개발

요구 사항: **Rust stable** · **Node 22** · **pnpm 9** · **WebView2**.

```bash
cd apps/desktop && pnpm install

cargo test --workspace --exclude worklog-app   # 코어 테스트
pnpm tauri dev                                 # 앱 실행 (apps/desktop 에서)
pnpm dev                                       # 브라우저 미리보기 → http://localhost:1420 (가짜 백엔드)
```

`pnpm dev` 는 Tauri 없이 UI 만 띄운다 — `src/mock.ts` 의 가짜 데이터로 화면을 확인할 때 쓴다.
릴리스 빌드(`pnpm tauri build`)에는 업데이터 서명을 위해 `TAURI_SIGNING_PRIVATE_KEY`(및 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`) 환경 변수가 필요하다.

## 릴리스

1. 저장소 secrets 에 `TAURI_SIGNING_PRIVATE_KEY` · `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` 를 등록한다.
2. `vX.Y.Z` 태그를 푸시한다.
3. `.github/workflows/release.yml` 이 설치기와 `latest.json` 을 GitHub Release 에 올린다. 기존 설치본은 앱 안에서 자동 업데이트된다.

## 구조

```
apps/desktop/            Tauri 2 셸(트레이·창·IPC·이벤트) + SolidJS UI
crates/worklog-core/     수집 · 분석 · 요약 · 저장 · 감시 · 스케줄
crates/worklog-cli/      `worklog` 명령
docs/v2-plan.md          설계 · 결정 기록
```

## 글꼴 · 라이선스

**나눔스퀘어라운드**(NAVER, OFL) · **주아**(우아한형제들, OFL) · **Gaegu**(OFL) 를 번들한다.
라이선스 파일은 `apps/desktop/src/assets/fonts` 에 함께 들어 있다.

## 변경 이력

- **v0.3.0** — 세션 정규화(worktree 중복 제거, 집중시간 구간 합집합), 세션 제목 정제(시스템 문구·Traceback·경로 제거), 지표 0 숨김과 커밋 0 이유, 근거 앵커·완료 단정 금지 프롬프트, 제외 목록(비공개 저장소는 요약에 보내지 않음)과 전송 고지, AI 요약 준비 배지, 공유용 복사, `worklog weekly` 주간 초안(CLI). Claude resume/fork 사본 세션 접기, 두 캘린더에 같이 있는 회의 중복 제거, 요약 타임아웃 설정(`summarizer.timeout_secs`, 기본 600초)과 실패 사유 표시·부분 요약 폴백.
- **v0.2.1** — 일지 템플릿 3종(표준·보고용·회고용), 일지 목록에 날짜별 지표, 지난 날짜 타임라인, 설정 › 모양(테마·글꼴·글자 크기), 손그림 룩. 설치기 이름을 `Worklog_…` 로(자동 업데이트용).
- **v0.2.0** — Rust + Tauri 2 로 전면 재작성 (Python v1 제거).
