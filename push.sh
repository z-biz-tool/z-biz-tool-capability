#!/usr/bin/env bash
# push.sh — 家族统一推送脚本（lead/pushAll.sh 批量调用，也可单仓独立运行）。
# 语义：只推已提交的当前分支（git push origin HEAD）。
# 红线：不 add / 不 commit / 不 stash / 不推 tag / 不 force；脏工作区原样保留。
# 用法：bash push.sh [--dry-run]
set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "${REPO_DIR}"
NAME="$(basename "${REPO_DIR}")"

if [[ ! -e .git ]]; then
  echo "❌ ${NAME}: 不是 git 仓（${REPO_DIR}）" >&2
  exit 1
fi

DRY_RUN=0
if [[ "${1:-}" == "--dry-run" ]]; then
  DRY_RUN=1
fi

BRANCH="$(git branch --show-current)"
if [[ -z "${BRANCH}" ]]; then
  echo "❌ ${NAME}: detached HEAD，无分支可推" >&2
  exit 1
fi

DIRTY="$(git status --porcelain --untracked-files=all | wc -l | tr -d '[:space:]')"

UPSTREAM=""
AHEAD="?"
BEHIND="?"
if UPSTREAM="$(git rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null)"; then
  AHEAD="$(git rev-list --count "${UPSTREAM}..HEAD")"
  BEHIND="$(git rev-list --count "HEAD..${UPSTREAM}")"
fi

echo "📦 ${NAME}: 分支=${BRANCH} 未推送=${AHEAD} 落后=${BEHIND} 未提交变更=${DIRTY}"

if [[ "${DRY_RUN}" == 1 ]]; then
  if [[ "${AHEAD}" != "?" ]] && [[ "${AHEAD}" -gt 0 ]]; then
    git log --oneline "${UPSTREAM}..HEAD"
  fi
  echo "🔍 ${NAME}: dry-run，未推送"
  exit 0
fi

if [[ "${BEHIND}" != "?" ]] && [[ "${BEHIND}" -gt 0 ]]; then
  echo "⚠️  ${NAME}: 本地落后远端 ${BEHIND} 个提交，仍尝试普通推送（预期被拒，绝不 force）"
fi

if git push origin HEAD; then
  echo "✅ ${NAME}: 推送完成"
else
  rc=$?
  echo "❌ ${NAME}: 推送失败（rc=${rc}）。不 force、不自动 rebase，请人工处理。" >&2
  exit "${rc}"
fi