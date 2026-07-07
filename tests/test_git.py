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


def test_scan_helpers_multiroot_depth_and_identity(tmp_path):
    """스캔 헬퍼: 여러 루트 병렬 탐색 + 깊이 제한 + git 식별/중복 제거."""
    import os

    from worklog.collectors.git_repos import _identify_many, _scan_many

    a = tmp_path / "A" / "repoA"
    b = tmp_path / "B" / "repoB"
    for d in (a, b):
        d.mkdir(parents=True)
        _git(d, "init", "-q")

    # 여러 루트를 병렬 탐색 → 둘 다 발견
    paths = _scan_many([str(tmp_path / "A"), str(tmp_path / "B")], depth=3)
    assert {os.path.basename(p) for p in paths} == {"repoA", "repoB"}

    # 깊이 1 은 두 단계 아래 저장소를 못 찾는다
    assert _scan_many([str(tmp_path)], depth=1) == []

    # 식별: 같은 경로를 중복 줘도 git-common-dir 로 하나로 합쳐진다
    idents = _identify_many([str(a), str(a)])
    assert len({canonical for _, canonical, _ in idents}) == 1


def test_git_collector_scan_roots_parallel(tmp_path):
    """scan_roots 아래 여러 저장소를 병렬 식별·로그하여 커밋을 모은다."""
    import os

    tz = get_tz("Asia/Seoul")
    today = datetime.now(tz)
    iso = today.replace(microsecond=0).isoformat()
    env = dict(os.environ, GIT_AUTHOR_DATE=iso, GIT_COMMITTER_DATE=iso)

    root = tmp_path / "roots"
    expected = set()
    for i in range(3):
        r = root / f"grp{i}" / f"repo{i}"
        r.mkdir(parents=True)
        _git(r, "init", "-q")
        _git(r, "config", "user.email", "me@x")
        _git(r, "config", "user.name", "Me")
        (r / "f.txt").write_text("a\n", encoding="utf-8")
        _git(r, "add", "f.txt")
        subprocess.run(["git", "-C", str(r), "commit", "-q", "-m", f"feat: c{i}"],
                       check=True, capture_output=True, text=True, env=env)
        expected.add(f"repo{i}")

    res = GitCollector(GitConfig(repos=[], scan_roots=[str(root)], scan_depth=5)).collect(_ctx(today.date()))
    assert res.ok
    assert {c.repo for c in res.data.commits} == expected
    assert len(res.data.commits) == 3


def test_git_scan_all_drives_uses_fixed_drives(tmp_path, monkeypatch):
    """scan_all_drives=True 면 scan_roots 대신 fixed_drives() 아래를 스캔한다."""
    import os

    tz = get_tz("Asia/Seoul")
    today = datetime.now(tz)
    iso = today.replace(microsecond=0).isoformat()
    env = dict(os.environ, GIT_AUTHOR_DATE=iso, GIT_COMMITTER_DATE=iso)

    repo = tmp_path / "drive" / "repoZ"
    repo.mkdir(parents=True)
    _git(repo, "init", "-q")
    _git(repo, "config", "user.email", "me@x")
    _git(repo, "config", "user.name", "Me")
    (repo / "f.txt").write_text("a\n", encoding="utf-8")
    _git(repo, "add", "f.txt")
    subprocess.run(["git", "-C", str(repo), "commit", "-q", "-m", "feat: z"],
                   check=True, capture_output=True, text=True, env=env)

    # 실제 하드디스크 대신 임시 폴더를 '드라이브'로 대체 (collector 는 호출 시점에 import)
    monkeypatch.setattr("worklog.util.fixed_drives", lambda: [str(tmp_path / "drive")])

    cfg = GitConfig(repos=[], scan_roots=[], scan_all_drives=True, scan_depth=3)
    res = GitCollector(cfg).collect(_ctx(today.date()))
    assert res.ok
    assert {c.repo for c in res.data.commits} == {"repoZ"}


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
