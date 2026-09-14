# 업무일지 v2 포팅 계획 — Rust + Tauri 2

작성 2026-09-14. 대상: 현재 Python 앱(v0.1.19)을 성능 우선 스택으로 전면 재작성한다.

## 0. 확정 사항

| 항목 | 결정 |
|---|---|
| 형태 | 로컬 전용 데스크톱 앱. 호스팅형 웹 없음(세션 로그·코드가 외부로 나가지 않음) |
| 백엔드 | Rust (`worklog-core` 라이브러리 + `worklog` CLI) |
| 셸 | Tauri 2 (WebView2). 트레이·알림·전역 단축키·자동 시작·단일 인스턴스·업데이터는 공식 플러그인 |
| UI | SolidJS + TypeScript + Vite. 와이어프레임 A 피드형 기준 (오늘 · 일지 · 설정 + 빠른 메모 창) |
| 저장소 | SQLite (`~/.worklog/worklog.db`, rusqlite bundled, WAL) |
| 실시간 | 행동 수집만 실시간(파일 감시 → Tauri 이벤트). 요약 문서는 사용자가 "지금 일지 만들기"를 눌렀을 때만 |
| 정해진 시각 | 설정에서 "알림만"(기본) / "자동 생성" 선택 |
| 배포 | NSIS 설치기 + `tauri-plugin-updater` (`latest.json`, minisign 서명). 포터블 exe 도 산출(업데이터 없음) |
| 저장 대상 | 설정에서 한 번 정하면 자동 저장. 편집한 일지는 "다시 생성" 시 확인 후에만 덮어씀 |

와이어프레임: https://claude.ai/code/artifact/0fd2a292-d7de-4027-bd9d-a71e428a42c2

## 1. 저장소 구조

같은 저장소 `main` 에서 진행한다. Python 코드는 컷오버(8단계)까지 그대로 두고, 그동안 `releases/worklog.exe` 는 계속 쓸 수 있다.

```
Cargo.toml                 # workspace
crates/
  worklog-core/            # 수집·분석·렌더·요약·저장·감시·스케줄 (라이브러리)
  worklog-cli/             # `worklog` 명령
apps/
  desktop/                 # Tauri 2 앱
    src/                   # SolidJS UI
    src-tauri/             # Rust 셸: 트레이·창·IPC 커맨드·이벤트
docs/
worklog/  tests/           # 기존 Python (컷오버 시 삭제)
```

## 2. 모듈 매핑 (Python → Rust)

| Python | Rust (`worklog-core`) | 비고 |
|---|---|---|
| `models.py` | `model.rs` | serde 파생. `ClaudeSession{agent}` 그대로 |
| `config.py` | `config.rs` | **settings.json 하나로 통합**. `config.yaml`/`.env` 지원 제거(v1 settings.json 키는 그대로 읽어 마이그레이션) |
| `util.py` | `time.rs`, `drives.rs`(windows-rs), `gitid.rs` | `resolve_day`, `parse_iso`, 고정 드라이브 열거, git-common-dir |
| `collectors/base.py` | `collect/mod.rs` | `Collector` trait, `CollectContext`, `CollectorResult{ok,skipped,warnings}` |
| `collectors/git_repos.py` | `collect/git.rs` + `scan.rs` | `gix` revwalk(HEAD+branches+remotes, committer date 기준 `[start,end)`, author 정규식 OR), numstat 은 커밋별 tree diff. 탐색은 `ignore::WalkParallel` |
| `collectors/claude_logs.py` | `collect/claude.rs` | 줄 단위 스트리밍, **오프셋 증분 파싱**, `_real_user_text` 규칙(isMeta·tool_result·합성 프롬프트 제외), 자정 carry, ai-title, file-history-snapshot |
| `collectors/codex_logs.py` | `collect/codex.rs` | `.jsonl` + `.jsonl.zst`(zstd), session_meta/response_item/event_msg, apply_patch 파일 추출, token_count 누적값은 마지막 값 |
| `collectors/naverworks.py` | `collect/naverworks.rs` | `jsonwebtoken` RS256 JWT-bearer, `reqwest`(rustls), 캘린더 다중·커서 페이지네이션, `calendar-personals` 목록 |
| `analyze.py` | `analyze.rs` | 커밋 타입 분류, KPI, 프로젝트 롤업, 하이라이트, 타임라인(회의 포함) |
| `render.py` | `render.rs` | facts / analysis / work signal / session blocks / `WORKLOG_SENTINEL` 메타 세션 필터 |
| `summarize.py` | `summarize.rs` | `claude -p --model` (tokio process, CREATE_NO_WINDOW) 또는 Anthropic API. map-reduce 동일 |
| `service.py` | `service.rs` | generate / save / history / `disambiguate_repo_names` |
| `outputs/*` | `output/{markdown,obsidian,notion}.rs` | Obsidian frontmatter 표식 보호 로직 유지 |
| `webapp/server.py`, `static/index.html` | `apps/desktop` | Tauri 커맨드 + Solid UI 로 대체 |
| `update.py`, `build_exe.bat`, `worklog.spec` | 삭제 | Tauri 번들러·업데이터 |
| `cli.py` | `worklog-cli` | 기존 옵션 유지 + `worklog note "..."` |
| (신규) | `store.rs` | SQLite: notes / runs / documents / repos / file_state / kv |
| (신규) | `watch.rs` | `notify` 감시: Claude projects, Codex sessions, 활성 저장소의 `.git/logs/HEAD`. 500ms 디바운스 |
| (신규) | `feed.rs` | 그날 이벤트 스냅샷(세션·커밋·회의·메모 통합 타임라인) 유지, 변경 시 델타 발행 |
| (신규) | `schedule.rs` | 정해진 시각 알림/자동 생성, 회의 폴링, 전체 재수집 주기 |

## 3. 데이터 (SQLite)

```sql
notes      (id, date, ts, text, tags_json, mentions_json, source, created, updated, deleted)
runs       (id, date, kind[manual|auto|retry], started, finished, status, error, duration_ms)
documents  (date PK, summary_md, full_md, generated_at, edited_at, run_id)
repos      (common_dir PK, path, name, last_seen, last_commit_ts, source[config|scan|claude])
file_state (path PK, offset, size, mtime, session_id)        -- jsonl 증분 파싱
kv         (key PK, value)                                    -- 마지막 전체 스캔 시각 등
```

마크다운 파일(`문서/업무일지/YYYY-MM-DD.md`, Obsidian, Notion)은 지금처럼 내보내기 결과다. 일지 화면은 `documents` 를 읽는다.

## 4. 실시간 파이프라인

1. 앱 시작: `repos` 캐시 로드 → 없으면 첫 전체 스캔(설정 루트 또는 고정 드라이브, 깊이 제한) → 캐시 저장.
2. 감시 대상: 최근 30일 활동 저장소 + 그날 Claude/Codex cwd 의 `.git/logs/HEAD`, `~/.claude/projects`, `~/.codex/sessions`.
3. 변경 이벤트 → 디바운스 → 해당 소스만 증분 수집 → `feed` 갱신 → Tauri `emit("feed:changed", delta)`.
4. 회의: 설정 주기(기본 15분) 폴링. 전체 재수집: 기본 30분. "지금 갱신" 버튼은 전체 재수집 즉시 실행.
5. 요약 생성은 작업 큐(동시 1개)로 실행하고 `emit("generate:progress", {step, detail})`.

성능 원칙: 프로세스 spawn 은 `claude` CLI 뿐이다. git 은 인프로세스(gix), jsonl 은 append 분만 읽는다. 창을 닫으면 WebView 를 내리고 Rust 프로세스만 트레이에 남긴다.

## 5. Tauri IPC

커맨드: `feed_today`, `note_add/edit/delete`, `generate_start/cancel`, `run_list`, `document_get/save/list/calendar`, `settings_get/set`, `test_connection(kind)`, `naverworks_calendars`, `open_folder`, `pick_path`, `rescan_repos`, `refresh_now`.
이벤트: `feed:changed`, `generate:progress`, `generate:done`, `reminder:fired`, `update:available`.

## 6. 단계와 완료 기준 (기능 단위 커밋)

| 단계 | 내용 | 완료 기준 |
|---|---|---|
| 0 툴체인·뼈대 | rustup 설치, workspace, `worklog-core`/`worklog-cli` 빈 크레이트, `apps/desktop` tauri init + Solid, GitHub Actions 빌드만 | `cargo build` · `pnpm tauri build` 성공 |
| 1 코어 기반 | model · config(마이그레이션) · time · store | Rust 테스트; v1 settings.json 을 읽어 동일 Config 산출 |
| 2 수집기 | claude → git → codex → naverworks 순 | Python `tests/` 의 수집기 테스트(≈35개)를 Rust 로 이식해 통과 |
| 3 파이프라인·CLI | analyze · render · summarize · outputs · service · CLI | 같은 날짜로 `worklog --dry-run --no-llm` 을 Python/Rust 양쪽에서 실행해 출력 diff 0 (골든 비교) |
| 4 메모·감시·스케줄 | notes · watch · feed · schedule | 커밋/세션 로그 변경 후 1초 내 feed 델타; 알림 시각 동작 |
| 5 Tauri 셸 | 트레이 · 단일 인스턴스 · 알림 · 자동 시작 · 전역 단축키(옵션) · 업데이터 · IPC | 창 닫기→트레이, 재실행→기존 창 앞으로, 알림 클릭→창 열림 |
| 6 UI | 오늘 · 일지 · 설정 · 빠른 메모 (와이어프레임 A) | 실제 하루 데이터로 세 화면 동작, 생성 진행 표시 |
| 7 배포 | NSIS 번들 · `latest.json` · 서명키 · CI 릴리스 | 태그 → 설치기 생성 → 이전 버전에서 업데이트 성공 |
| 8 컷오버 | Python·PyInstaller·update.py 삭제, README, v0.2.0 | 저장소에 Python 없음, 릴리스 exe 교체 |

각 단계는 하나 이상의 기능 단위 커밋으로 끝내고, 3단계 골든 비교를 통과하기 전에는 Python 코드를 지우지 않는다.

## 7. 성능 검증 항목 (3·4단계에서 측정)

- 첫 전체 스캔 시간(고정 드라이브 전체, 깊이 5) 과 캐시 후 시작 시간
- 저장소 100개 기준 그날 커밋 수집 시간 (Python subprocess 방식과 비교)
- 10MB jsonl 전체 파싱 vs 증분 파싱 시간
- 파일 변경 → UI 반영 지연
- 트레이 상주 메모리(창 닫힘 상태)

## 8. 결정 (2026-09-14)

1. **GitHub 저장소 `kariseio/worklog` 는 공개.** 업데이트 endpoint 는 `https://github.com/kariseio/worklog/releases/latest/download/latest.json`.
2. **설정 파일 통합.** `config.yaml`/`.env` 를 버리고 `~/.worklog/settings.json` 하나로 간다. CLI 도 같은 파일을 읽는다.
3. **Windows 코드 서명은 보류.** 사내 사용이라 SmartScreen 경고를 감수한다. 필요해지면 CI 에 서명 단계만 추가.
4. **전역 단축키(Ctrl+Shift+Space)는 1차에 포함.** `tauri-plugin-global-shortcut`.

## 9. 진행 상황

| 단계 | 상태 | 비고 |
|---|---|---|
| 0 툴체인·뼈대 | 완료 (2026-09-14) | `cargo build --workspace` 1m57s(첫 빌드), `pnpm tauri build` → `업무일지_0.2.0_x64-setup.exe`(1.5MB), 단독 exe 5.1MB, 창 뜬 상태 메인 프로세스 25MB. CI: `.github/workflows/build.yml` |
| 1 코어 기반 | 완료 (2026-09-14, `38afc15`) | model·config·time·paths·store. 4관점 적대적 리뷰 20건 반영. 실제 `settings.json` 을 Python/Rust 로더로 읽어 39개 필드 일치 |
| 2 수집기 | 완료 (2026-09-14, `097afbf`) | claude·codex·git(gix)·naverworks·scan·drives. Python 수집기 테스트 이식 |
| 3 파이프라인·CLI | 완료 (2026-09-14, `d1a7de1`) | analyze·render·summarize·output·service·CLI. 골든 비교: 9/10·9/11·9/12 `--dry-run --no-llm` 출력이 Python 과 동일(끝의 빈 줄 1개 제외). 같은 날짜 소요: Python 13~24초 → Rust 1.1~3.5초. 코어 테스트 100개 |
| 4 메모·감시·스케줄 | 완료 (2026-09-14) | notes(메모 → 파이프라인·CLI `note`/`notes`), feed(피드·델타), watch(notify 감시·디바운스), live(소스별 증분 갱신 엔진), schedule(발동 시각 계산). 코어 테스트 112개 |
| 5 Tauri 셸 | 완료 (2026-09-14) | 상태·엔진 스레드·생성 작업·IPC 30개·플러그인 7종(단일 인스턴스·알림·자동 시작·전역 단축키·업데이터·대화상자·열기). 4관점 적대적 리뷰 22건 반영. 스모크: 시작 수집 3.0s(항목 26·저장소 92), 세션 로그 변경 → 피드 반영 50~100ms, 두 번째 실행 0.3~1.1s 만에 기존 창으로, 창 닫기 후 프로세스 상주(private 21MB · working set 114MB, gix mmap 포함), 재실행 시 창 재생성 확인. 알림 클릭→창 열림은 설치기(시작 메뉴 바로가기 AUMID) 뒤 7단계에서 확인 |

### 셸 결정 사항(5단계에서 확정)
- **스레드 모델**: 엔진 스레드 하나가 `Live` 와 파일 감시 핸들을 독점한다. 커맨드는 `AppState` 의 피드 스냅샷만 읽고, 메모·설정 변경은 mpsc 로 엔진에 알린다. 엔진은 SQLite 연결을 따로 연다(WAL). 생성 작업은 별도 스레드, 동시 1개, `catch_unwind` 로 감싼다 — 그래서 릴리스 프로필의 `panic = "abort"` 를 뺐다(abort 면 모든 패닉 폴백이 죽는다).
- **저장소 캐시(`repos`)**: 시작 때 캐시로 먼저 채우고(빠른 첫 화면) 이어서 디스크를 한 번 탐색해 캐시를 갱신한다. 주기 전체 수집(30분)은 캐시 + 세션 cwd 만 다시 읽고 디스크는 안 본다. 디스크 재탐색은 시작·'저장소 다시 찾기'(`rescan_repos`)·git 설정 변경 때만. 세션 파일이 바뀌면 그 cwd 가 새 저장소인지 바로 확인해 감시에 넣는다.
- **회의 폴링·전체 재수집은 실시간(파일 감시) 설정과 무관하게 돈다.** 실시간을 끄면 파일 감시만 사라진다.
- **창**: 메인 창 닫기 = 창 파괴(WebView 해제), 트레이·엔진은 남고 다시 열면 설정 파일 정의로 재생성. **빠른 메모 창은 숨긴 채 상주**(단축키에 즉시 떠야 함 — 계획 §4 의 예외). 포커스를 잃으면 150ms 뒤 여전히 포커스가 없을 때만 숨긴다(WebView2 의 일시적 blur 대응).
- **전역 단축키**: `unregister_all` 은 플러그인 뮤텍스를 쥔 채 메인 스레드를 기다려 핫키 핸들러와 잠금 순서가 엇갈릴 수 있어 쓰지 않는다. 이전 키 `unregister` → 새 키 `register`. 등록 오류는 `AppState` 에 남겨 설정 화면에서 보여 주고, 저장할 때마다 다시 시도한다.
- **편집된 일지 보호**: `generate_start` 는 `documents.edited_at` 이 있으면 `overwrite_edited` 없이는 거절한다(오류 문구 접두어 "편집된 일지가 있습니다" → 화면이 확인 후 재호출). 정해진 시각 자동 생성은 절대 덮어쓰지 않고 알림만.
- **취소**: 취소 플래그가 `Summarizer` → claude CLI 자식 프로세스까지 내려가 즉시 kill. 상태는 '취소 중'으로 바뀌고, 문서는 저장하지 않는다.
- **설정 저장 안전장치**: 시작 때 설정 파일을 못 읽었으면(`ReadFailed`) 저장 시 한 번 더 읽어 보고, 읽히면 "화면을 새로 고친 뒤 다시 저장" 오류로 돌려보낸다(기본값으로 덮어쓰기 방지). 여전히 못 읽으면 저장 거부.
- **놓친 정해진 시각 동작**: 엔진이 1분마다 `kv.app.last_alive` 를 남기고, 시작 때 `due_between(last_alive, now)` 로 놓친 발동(24시간 안, 마지막 처리 이후)을 한 번만 처리한다.
- **엔진 요청 병합**: 쌓인 요청은 한 번에 꺼내 합친다(설정 변경은 마지막 것만, 전체 수집이 있으면 부분 갱신 생략, 변경 배치는 이어 붙임).
- **메모는 틱(60초)마다 다시 읽는다** — CLI `worklog note` 로 넣은 메모도 1분 안에 피드에 뜬다.
- 업데이터 `pubkey` 는 7단계에서 서명키를 만들 때 채운다. 그때까지 `update_check` 는 오류를 돌려주고 시작 시 자동 확인은 조용히 실패한다.

### 파이프라인 결정 사항(3단계에서 확정)
- 캘린더(NaverWorks)의 오프셋 없는 시각은 **설정 시간대의 로컬 시각**으로 해석(`time::parse_iso_in`). v1 은 시스템 로컬로 해석했고 결과는 같다. Claude/Codex 타임스탬프는 항상 `Z` 라 무관.
- git 로그는 gix `ByCommitTime(NewestFirst)` 로 걷다가 대상일보다 오래된 커밋이 연속 5개 나오면 멈춘다(git `--since` 의 slop 과 동일). `ByCommitTimeCutoff` 는 amend 로 자식이 부모보다 오래된 경우 오늘 커밋을 놓쳐 쓰지 않는다.
- numstat 은 첫 부모와의 tree diff 로 계산하고 바이너리는 파일 수에만 든다. 실제 저장소 3곳에서 `git log --numstat` 합계와 일치했다.
- 저장 마크다운은 LF 로 쓴다(v1 은 Windows 텍스트 모드라 CRLF). Obsidian·Notion 모두 무관.

## 10. 이 PC 준비 상태 (2026-09-14 확인)

| 항목 | 상태 |
|---|---|
| Rust (rustup/cargo) | 0단계에서 설치함 (stable 1.98.1, MSVC). `%USERPROFILE%\.cargo\bin` 을 사용자 PATH 에 추가 |
| MSVC Build Tools 2022 + Windows SDK 10.0.22621/26100 | 있음 |
| WebView2 런타임 | 131 있음 |
| Node 22 / pnpm 9 | 있음 |
| git 2.45 / claude CLI | 있음 |
