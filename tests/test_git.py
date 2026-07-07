"""Git 수집기 테스트 (실제 임시 저장소 사용). git 없으면 skip."""

from __future__ import annotations

import logging
import shutil
import subprocess
from datetime import date, datetime, timedelta

import pytest

from worklog.collectors.base import CollectContext
from worklog.collectors.git_repos import GitCollector
from worklog.config import GitConfig
from worklog.util import get_tz, resolve_day

pytestmark = pytest.mark.skipif(shutil.which("git") is None, reason="git 미설치")


def _ctx(target: date) -> CollectContext:
    tz = get_tz("Asia/Seoul")
    t, start, end = resolve_day(target.isoformat(), tz)
    return CollectContext(target_date=t, start=start, end=end, tz=tz,
                          tz_name="Asia/Seoul", logger=logging.getLogger("test"))


def _git(repo, *args, env=None):
    subprocess.run(["git", "-C", str(repo), *args], check=True,
                   capture_output=True, text=True, env=env)


def test_collects_todays_commit(tmp_path):
    import os

    repo = tmp_path / "repo"
    repo.mkdir()
    _git(repo, "init", "-q")
    _git(repo, "config", "user.email", "me@example.com")
    _git(repo, "config", "user.name", "Me")

    (repo / "a.txt").write_text("hello\nworld\n", encoding="utf-8")
    _git(repo, "add", "a.txt")

    today = datetime.now(get_tz("Asia/Seoul"))
    iso = today.replace(microsecond=0).isoformat()
    env = dict(os.environ, GIT_AUTHOR_DATE=iso, GIT_COMMITTER_DATE=iso)
    subprocess.run(["git", "-C", str(repo), "commit", "-q", "-m", "feat: 첫 커밋"],
                   check=True, capture_output=True, text=True, env=env)

    coll = GitCollector(GitConfig(repos=[str(repo)]))
    res = coll.collect(_ctx(today.date()))

    assert res.ok
    assert len(res.data.commits) == 1
    c = res.data.commits[0]
    assert c.subject == "feat: 첫 커밋"
    assert c.insertions == 2
    assert c.files_changed == 1
    assert c.repo == "repo"


def test_uses_committer_date_not_author(tmp_path):
    """amend/rebase 처럼 저자날짜(어제)≠커밋날짜(오늘)면 커밋날짜 기준으로 잡히고 표시돼야."""
    import os

    repo = tmp_path / "repo"
    repo.mkdir()
    _git(repo, "init", "-q")
    _git(repo, "config", "user.email", "me@example.com")
    _git(repo, "config", "user.name", "Me")
    (repo / "a.txt").write_text("x\n", encoding="utf-8")
    _git(repo, "add", "a.txt")

    tz = get_tz("Asia/Seoul")
    today = datetime.now(tz)
    a_iso = (today - timedelta(days=1)).replace(microsecond=0).isoformat()   # 저자=어제
    c_iso = today.replace(microsecond=0).isoformat()                          # 커밋=오늘
    env = dict(os.environ, GIT_AUTHOR_DATE=a_iso, GIT_COMMITTER_DATE=c_iso)
    subprocess.run(["git", "-C", str(repo), "commit", "-q", "-m", "amended"],
                   check=True, capture_output=True, text=True, env=env)

    res = GitCollector(GitConfig(repos=[str(repo)])).collect(_ctx(today.date()))
    assert len(res.data.commits) == 1
    assert res.data.commits[0].when.astimezone(tz).date() == today.date()   # 커밋날짜=오늘


def test_no_repos_skips():
    coll = GitCollector(GitConfig(repos=[], scan_roots=[]))
    res = coll.collect(_ctx(date(2026, 7, 6)))
    assert res.skipped


def test_disambiguates_same_named_repos(tmp_path):
    """다른 경로의 동명 저장소는 상위 폴더로 구분되어야 한다."""
    from datetime import datetime, timezone

    from worklog.models import ClaudeData, ClaudeSession, DailyData, GitCommit, GitData
    from worklog.service import disambiguate_repo_names
    from worklog.util import git_common_dir

    a = tmp_path / "aa" / "app"
    b = tmp_path / "bb" / "app"
    for d in (a, b):
        d.mkdir(parents=True)
        _git(d, "init", "-q")
    ka, kb = git_common_dir(str(a)), git_common_dir(str(b))
    assert ka and kb and ka != kb

    data = DailyData(target_date=date(2026, 7, 6), tz_name="Asia/Seoul")
    data.git = GitData(commits=[
        GitCommit(repo="app", hash="x", author="m", when=datetime(2026, 7, 6, tzinfo=timezone.utc),
                  subject="s", repo_path=ka),
        GitCommit(repo="app", hash="y", author="m", when=datetime(2026, 7, 6, tzinfo=timezone.utc),
                  subject="t", repo_path=kb),
    ])
    data.claude = ClaudeData(sessions=[
        ClaudeSession(session_id="1", project="app", cwd=str(a), git_branch=None,
                      title="t", intent=None),
    ])
    disambiguate_repo_names(data)

    assert {c.repo for c in data.git.commits} == {"aa/app", "bb/app"}
    # 같은 물리 저장소(a)의 Claude 세션은 git 커밋과 같은 이름으로 매칭
    assert data.claude.sessions[0].project == "aa/app"


def test_single_repo_name_unchanged(tmp_path):
    from datetime import datetime, timezone

    from worklog.models import DailyData, GitCommit, GitData
    from worklog.service import disambiguate_repo_names
    from worklog.util import git_common_dir

    r = tmp_path / "solo"
    r.mkdir()
    _git(r, "init", "-q")
    k = git_common_dir(str(r))
    data = DailyData(target_date=date(2026, 7, 6), tz_name="Asia/Seoul")
    data.git = GitData(commits=[GitCommit(repo="solo", hash="x", author="m",
                                          when=datetime(2026, 7, 6, tzinfo=timezone.utc),
                                          subject="s", repo_path=k)])
    disambiguate_repo_names(data)
    assert data.git.commits[0].repo == "solo"   # 충돌 없으면 그대로
