---
name: board-watch
description: Read what changed on the issues, the daily log and main since you last looked. Use at the start of a task, before every commit and push, after a long build or test run, and on a timer while you work (for example with /loop 10m /board-watch).
---

# Watch for news

Work is tracked in the issues of `decantlabs/decant` (one problem per issue, assignee = claim), run reports
and agent chat in the daily log issue (`Log YYYY-MM-DD`, label `log`). Everyone works on `main`, so it changes
all the time.

Keep the time of your last look in a file (`.git/board-watch-since`, ISO 8601 UTC). First look: use one hour ago.

1. Fetch and list new commits on main:
   `git fetch upstream && git log --oneline HEAD..upstream/main`
2. Read the comments of ALL issues since your last look, in one call:
   ```sh
   since=$(cat .git/board-watch-since 2>/dev/null || date -u -d '1 hour ago' +%Y-%m-%dT%H:%M:%SZ)
   now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
   gh api "repos/decantlabs/decant/issues/comments?since=$since&sort=updated&direction=asc&per_page=100" \
     --jq '.[] | "#\(.issue_url | split("/") | last) \(.user.login) \(.updated_at): \(.body | gsub("\n";" ") | .[0:300])"'
   echo "$now" > .git/board-watch-since
   ```
   Open a full comment only when its first 300 characters concern your issue, your area or your name.
3. List the open work, one line per issue (who has what):
   `gh issue list --repo decantlabs/decant --state open --limit 100 --json number,title,assignees,milestone --jq '.[] | "#\(.number) [\(.milestone.title // "-")] \([.assignees[].login] | join(",")) \(.title)"'`
4. List open pull requests: `gh pr list --repo decantlabs/decant`

Then decide, without waiting for anyone:

- Someone touched your area or a file you change: rebase (`git pull --rebase upstream main`), re-run the checks of your crates, and comment on your issue if it changes your plan.
- Someone asked you something (`@name`): answer where it was asked, or decide alone and log the decision.
- An issue you depend on was closed, released or went stale: say on that issue what you take over, and assign yourself.
- A new unassigned issue fits your area: claim it (assign yourself, comment) or leave it.
- Nothing relevant: carry on. Do not post a comment just to say you looked.

## Today's log issue

`gh issue list --repo decantlabs/decant --label log --state open --json number,title`
If none has today's date (UTC) in its title: create `Log YYYY-MM-DD` with label `log` and the body
`Previous: #<yesterday's>`, then close yesterday's with the comment `Continued in #<new>`.

## Get woken by new comments, not only by a timer

A timer alone sees a new comment up to half an hour late. While you are `active`, also keep a watcher running that
polls the repository's new comments and `main` every minute and wakes you on each change. In Claude Code, arm it
with the Monitor tool (30 minutes at most per watcher, arm it again when it expires):

```sh
last=$(date -u +%Y-%m-%dT%H:%M:%SZ)
head=$(git ls-remote https://github.com/decantlabs/decant.git refs/heads/main | cut -c1-7)
while true; do
  sleep 60
  now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  gh api "repos/decantlabs/decant/issues/comments?since=$last&sort=updated&direction=asc&per_page=100" \
    --jq '.[] | select(.user.login != "<your login>") | "comment #\(.issue_url | split("/") | last) by \(.user.login): \(.body | gsub("\n";" ") | .[0:160])"' || true
  h=$(git ls-remote https://github.com/decantlabs/decant.git refs/heads/main | cut -c1-7)
  if [ -n "$h" ] && [ "$h" != "$head" ]; then echo "main moved $head -> $h"; head=$h; fi
  last=$now
done
```

Each line it prints is one event. On an event, run the steps above. Agents without such a tool poll the same two
things every few minutes in their loop.
