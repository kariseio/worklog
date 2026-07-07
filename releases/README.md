# releases

버전별 단일 실행파일(.exe) 보관 폴더.

- 파일명: `worklog-<버전>.exe` (예: `worklog-0.1.0.exe`)
- `build_exe.bat` 를 실행하면 빌드 후 `pyproject.toml` 의 버전으로 이 폴더에 **자동 복사**된다.
- `.exe` 바이너리는 용량 때문에 git 에 커밋하지 않는다(`.gitignore` 에서 `releases/*.exe` 제외).
  원격 배포는 태그(`vX.Y.Z`)에 맞춰 **GitHub Releases** 에 첨부하는 방식을 권장.

## 버전 목록
- `worklog-0.1.1.exe` — v0.1.1 (설정 UI 개편[좌측 탭·스크롤 잠금·버전/경로]·수집 소스 단순화[스캔 범위 on/off, 물리 드라이브만 선택]·수집 병렬화·AI 요약 대기 표시 버그픽스)
- `worklog-0.1.0.exe` — v0.1.0 (수집·시간대별 요약·지표/타임라인·Markdown/Obsidian/Notion·데스크톱 앱·트레이·단일 인스턴스)
