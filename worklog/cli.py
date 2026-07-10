"""명령행 진입점.

    worklog                     # 오늘 업무일지 생성
    worklog --date 2026-07-05
    worklog --yesterday
    worklog --no-llm            # LLM 요약 없이 데이터만
    worklog --sources git,claude
    worklog --dry-run           # 파일로 저장하지 않고 콘솔에 출력
    worklog --init              # config.yaml / .env 템플릿 생성
    worklog --app               # 데스크톱 앱 실행
"""

from __future__ import annotations

import argparse
import logging
import shutil
import sys
from pathlib import Path

from . import __version__, service
from .config import EXAMPLE_CONFIG_PATH, EXAMPLE_ENV_PATH, load_config
from .util import setup_logging

log = logging.getLogger("worklog")


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="worklog", description="업무일지 생성기")
    p.add_argument("--date", help="대상 날짜 (YYYY-MM-DD | today | yesterday)")
    p.add_argument("--yesterday", action="store_true", help="어제로 설정")
    p.add_argument("--config", help="config.yaml 경로 (기본: ./config.yaml)")
    p.add_argument("--tz", help="시간대 재정의 (예: Asia/Seoul)")
    p.add_argument("--sources", help="사용할 소스만 콤마로 (git,claude,codex,naverworks)")
    p.add_argument("--no-llm", action="store_true", help="LLM 요약 생략")
    p.add_argument("--dry-run", action="store_true", help="파일 저장 없이 콘솔 출력")
    p.add_argument("--init", action="store_true", help="config.yaml / .env 템플릿 생성")
    p.add_argument("--app", action="store_true", help="데스크톱 앱(GUI) 실행")
    p.add_argument("-v", "--verbose", action="store_true")
    p.add_argument("--version", action="version", version=f"worklog {__version__}")
    return p


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    setup_logging(args.verbose)

    if args.init:
        return _init_templates()

    if args.app:
        from .webapp.launcher import run_app

        return run_app(config_path=args.config)

    cfg = load_config(args.config)
    if args.tz:
        cfg.timezone = args.tz

    date_str = "yesterday" if args.yesterday else args.date
    if date_str and date_str not in ("today", "yesterday"):
        from datetime import date as _date
        try:
            _date.fromisoformat(date_str)
        except ValueError:
            log.error("날짜 형식 오류: --date 는 YYYY-MM-DD | today | yesterday 여야 합니다. 입력값: %r", date_str)
            return 2

    log.info("업무일지 생성 (%s)", cfg.timezone)
    result = service.generate(cfg, date_str=date_str, no_llm=args.no_llm, sources=args.sources)
    worklog = result.worklog

    if worklog.data.is_empty():
        log.info("수집된 데이터가 없습니다.")

    if args.dry_run:
        print(worklog.full_markdown)
        return 0

    _write(cfg, worklog)
    return 0


def _write(cfg, worklog) -> None:
    results = service.save(cfg, worklog)
    if not results:
        log.warning("활성화된 출력이 없습니다. config.yaml 의 outputs 를 확인하세요.")
        return
    print()
    for r in results:
        if r.ok:
            print(f"  ✅ {r.name}: {r.location}")
        else:
            print(f"  ❌ {r.name}: {r.error}")


def _init_templates() -> int:
    created = []
    for src, dst in [
        (EXAMPLE_CONFIG_PATH, Path("config.yaml")),
        (EXAMPLE_ENV_PATH, Path(".env")),
    ]:
        if dst.exists():
            print(f"  (건너뜀) 이미 존재: {dst}")
            continue
        if not src.exists():
            print(f"  ⚠️ 템플릿 없음: {src}")
            continue
        shutil.copyfile(src, dst)
        created.append(str(dst))
        print(f"  ✅ 생성: {dst}")
    if created:
        print("\nconfig.yaml 과 .env 를 열어 값을 채운 뒤 `worklog` 를 실행하세요.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
