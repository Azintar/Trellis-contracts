#!/usr/bin/env node

import { execSync } from 'node:child_process';

const REPO_OWNER = process.env.GITHUB_REPOSITORY ? process.env.GITHUB_REPOSITORY.split('/')[0] : 'TRELLIS-STELLAR';
const REPO_NAME = process.env.GITHUB_REPOSITORY ? process.env.GITHUB_REPOSITORY.split('/')[1] : 'Trellis-contracts';

const GITHUB_TOKEN =
  process.env.DORIS_PAT ||
  process.env.GITHUB_PAT ||
  process.env.GH_TOKEN ||
  process.env.GITHUB_TOKEN;

function runCmd(cmd, options = {}) {
  try {
    return execSync(cmd, { encoding: 'utf-8', stdio: ['pipe', 'pipe', 'pipe'], ...options }).trim();
  } catch (err) {
    const error = new Error(`Command failed: ${cmd}\n${err.stderr || err.message}`);
    error.stdout = err.stdout;
    error.stderr = err.stderr;
    error.code = err.status;
    throw error;
  }
}

async function apiFetch(endpoint, method = 'GET', body = null) {
  const headers = {
    Accept: 'application/vnd.github+json',
    'User-Agent': 'Trellis-Contracts-Doris-Auto-Merger',
    ...(GITHUB_TOKEN ? { Authorization: `Bearer ${GITHUB_TOKEN}` } : {}),
  };

  const res = await fetch(`https://api.github.com/repos/${REPO_OWNER}/${REPO_NAME}${endpoint}`, {
    method,
    headers: {
      ...headers,
      ...(body ? { 'Content-Type': 'application/json' } : {}),
    },
    ...(body ? { body: JSON.stringify(body) } : {}),
  });

  const text = await res.text();
  let data;
  try { data = text ? JSON.parse(text) : null; } catch { data = text; }
  return { status: res.status, ok: res.ok, data };
}

async function fetchAllOpenIssues() {
  const issues = [];
  try {
    let page = 1;
    while (true) {
      const res = await apiFetch(`/issues?state=open&per_page=100&page=${page}`);
      if (!res.ok || !res.data || res.data.length === 0) break;
      for (const item of res.data) {
        if (!item.pull_request) {
          issues.push(item);
        }
      }
      if (res.data.length < 100) break;
      page++;
    }
  } catch (err) {
    console.warn(`  ! Could not fetch open issues for link matching: ${err.message}`);
  }
  return issues;
}

function findLinkedIssues(pr, allOpenIssues) {
  const linked = new Set();
  const text = `${pr.title} ${pr.body || ''} ${pr.head.ref}`;

  // 1. Look for explicit keyword patterns: (close|closes|closed|fix|fixes|fixed|resolve|resolves|resolved) #(\d+)
  const keywordRegex = /(?:close|closes|closed|fix|fixes|fixed|resolve|resolves|resolved)\s*#?(\d+)/gi;
  let match;
  while ((match = keywordRegex.exec(text)) !== null) {
    linked.add(parseInt(match[1], 10));
  }

  // 2. Look for range patterns in branch name or text, e.g. "82-86" or "85-89"
  const rangeRegex = /(\d+)-(\d+)/g;
  while ((match = rangeRegex.exec(pr.head.ref)) !== null) {
    const start = parseInt(match[1], 10);
    const end = parseInt(match[2], 10);
    if (end >= start && end - start <= 15) {
      for (let i = start; i <= end; i++) {
        linked.add(i);
      }
    }
  }

  // 3. Look for issue numbers in branch name: e.g. "issue-28" or "101-verify" or "/35-"
  const branchNumRegex = /(?:issue-|issue|\/|^)(\d+)(?:-|\/|$)/gi;
  while ((match = branchNumRegex.exec(pr.head.ref)) !== null) {
    linked.add(parseInt(match[1], 10));
  }

  // 4. Look for raw #(\d+) in title or body
  const rawHashRegex = /#(\d+)/g;
  while ((match = rawHashRegex.exec(text)) !== null) {
    linked.add(parseInt(match[1], 10));
  }

  // 5. Match against open issues by title similarity / keywords
  if (linked.size === 0 && allOpenIssues && allOpenIssues.length > 0) {
    const cleanTitle = pr.title.toLowerCase().replace(/^(feat|fix|chore|docs|refactor)(\([^)]+\))?:\s*/, '').trim();
    for (const issue of allOpenIssues) {
      const cleanIssueTitle = issue.title.toLowerCase().replace(/^(trellis contracts:\s*)/, '').trim();
      if (cleanTitle.length > 8 && (cleanTitle.includes(cleanIssueTitle) || cleanIssueTitle.includes(cleanTitle))) {
        linked.add(issue.number);
      }
    }
  }

  return Array.from(linked);
}

async function approvePR(prNumber, linkedIssues) {
  if (!GITHUB_TOKEN) return false;
  try {
    const closesText = linkedIssues.length > 0
      ? `\n\nLinked issue(s): ${linkedIssues.map(i => `#${i}`).join(', ')}`
      : '';
    const res = await apiFetch(`/pulls/${prNumber}/reviews`, 'POST', {
      event: 'APPROVE',
      body: `Auto-approved by Doris Maduegbunam (Maintainer).${closesText}`,
    });
    return res.ok;
  } catch (err) {
    console.warn(`  ! Could not submit approval review for PR #${prNumber}: ${err.message}`);
    return false;
  }
}

async function mergeViaAPI(prNumber, commitTitle, commitMessage) {
  if (!GITHUB_TOKEN) return false;

  const res = await apiFetch(`/pulls/${prNumber}/merge`, 'PUT', {
    commit_title: commitTitle,
    commit_message: commitMessage,
    merge_method: 'merge',
  });

  return res.ok;
}

async function linkAndCloseIssueViaAPI(issueNumber, prNumber) {
  if (!GITHUB_TOKEN) return false;
  try {
    // Post comment referencing the PR
    await apiFetch(`/issues/${issueNumber}/comments`, 'POST', {
      body: `Resolved and merged by @dorismaduegbunam via PR #${prNumber}.`,
    });
    // Close the issue
    await apiFetch(`/issues/${issueNumber}`, 'PATCH', {
      state: 'closed',
      state_reason: 'completed',
    });
    return true;
  } catch (err) {
    console.warn(`  ! Could not update issue #${issueNumber} via API: ${err.message}`);
    return false;
  }
}

async function main() {
  console.log(`[Doris Maintainer Auto-Merge Engine] Target Repository: ${REPO_OWNER}/${REPO_NAME}`);
  console.log('Maintainer Identity: Doris Maduegbunam <dorismaduegbunam@gmail.com>');

  console.log('Fetching open Pull Requests from GitHub...');
  const res = await fetch(`https://api.github.com/repos/${REPO_OWNER}/${REPO_NAME}/pulls?state=open&per_page=100`, {
    headers: { 'User-Agent': 'Trellis-Contracts-Doris-Auto-Merger' }
  });

  if (!res.ok) {
    console.error(`Failed to list open PRs: HTTP ${res.status}`);
    process.exit(1);
  }

  const prs = await res.json();
  console.log(`Found ${prs.length} open Pull Request(s).`);

  if (prs.length === 0) {
    console.log('No open PRs to merge. Exiting cleanly.');
    return;
  }

  // Fetch open issues to accurately link issues
  console.log('Fetching open issues for intelligent PR-to-Issue linking...');
  const openIssues = await fetchAllOpenIssues();
  console.log(`Loaded ${openIssues.length} open issue(s) for cross-referencing.`);

  // Sort by PR number ascending
  prs.sort((a, b) => a.number - b.number);

  let isGitRepo = false;
  try {
    runCmd('git rev-parse --is-inside-work-tree');
    isGitRepo = true;
  } catch {}

  if (isGitRepo) {
    console.log('\nSetting up local git state under Doris Maduegbunam identity...');
    try {
      runCmd('git config user.name "Doris Maduegbunam"');
      runCmd('git config user.email "dorismaduegbunam@gmail.com"');
      runCmd('git checkout main');
      runCmd('git pull origin main');
    } catch (err) {
      console.warn(`Git setup notice: ${err.message}`);
    }
  }

  const merged = [];
  const skipped = [];

  for (const pr of prs) {
    const prNum = pr.number;
    const author = pr.user ? pr.user.login : 'unknown';
    const linkedIssues = findLinkedIssues(pr, openIssues);

    const closesClause = linkedIssues.length > 0
      ? '\n\n' + linkedIssues.map((id) => `Closes #${id}`).join('\n')
      : '';

    const commitTitle = `Merge pull request #${prNum} from ${author}/${pr.head.ref} - ${pr.title}`;
    const commitBody = `${pr.title}${closesClause}`;
    const fullCommitMessage = `${commitTitle}\n\n${commitBody}`;

    console.log(`\n------------------------------------------------------------`);
    console.log(`Processing PR #${prNum}: "${pr.title}" (@${author})`);
    if (linkedIssues.length > 0) {
      console.log(`  🔗 Identified linked issue(s): ${linkedIssues.map(i => '#' + i).join(', ')}`);
    } else {
      console.log(`  ℹ No explicit linked issue found.`);
    }
    console.log(`------------------------------------------------------------`);

    // 1. Submit review approval as Doris
    if (GITHUB_TOKEN) {
      await approvePR(prNum, linkedIssues);
    }

    // 2. Strategy A: Try API merge with Doris PAT if available
    let apiSuccess = false;
    if (GITHUB_TOKEN) {
      try {
        apiSuccess = await mergeViaAPI(prNum, commitTitle, commitBody);
        if (apiSuccess) {
          console.log(`  ✓ Successfully merged PR #${prNum} via GitHub API as Doris.`);
          for (const issueNum of linkedIssues) {
            await linkAndCloseIssueViaAPI(issueNum, prNum);
          }
          merged.push({ number: prNum, title: pr.title, author, method: 'API-Doris', linkedIssues });
          continue;
        } else {
          console.log(`  ! API merge returned non-OK status, falling back to Git merge via Doris SSH...`);
        }
      } catch (apiErr) {
        console.warn(`  Notice: API merge failed (${apiErr.message}), falling back to Git merge via Doris SSH...`);
      }
    }

    // 3. Strategy B: Git SSH / Local Merge fallback using Doris's Git & SSH credentials
    if (isGitRepo) {
      const branchName = `auto-pr-${prNum}`;
      try {
        console.log(`  Fetching refs/pull/${prNum}/head...`);
        runCmd(`git fetch origin pull/${prNum}/head:${branchName} --force`);

        console.log(`  Merging ${branchName} into main with issue-linking message...`);
        const escapedMessage = fullCommitMessage.replace(/"/g, '\\"');
        try {
          runCmd(`git merge ${branchName} --no-ff -m "${escapedMessage}"`);
          console.log(`  ✓ Merged PR #${prNum} locally via Doris git identity with issue links.`);
          merged.push({ number: prNum, title: pr.title, author, method: 'Git-Doris', linkedIssues });
        } catch {
          console.warn(`  ! Conflict encountered on PR #${prNum}. Attempting -X ours resolution...`);
          try { runCmd('git merge --abort'); } catch {}
          try {
            runCmd(`git merge ${branchName} --no-ff -X ours -m "${escapedMessage}"`);
            console.log(`  ✓ Merged PR #${prNum} (resolved via -X ours with issue links).`);
            merged.push({ number: prNum, title: pr.title, author, method: 'Git-Ours-Doris', linkedIssues });
          } catch (retryErr) {
            try { runCmd('git merge --abort'); } catch {}
            console.error(`  ✗ Skipping PR #${prNum}: persistent merge conflict.`);
            skipped.push({ number: prNum, title: pr.title, reason: retryErr.message });
          }
        }
      } catch (err) {
        console.error(`  ✗ Error fetching/merging PR #${prNum}: ${err.message}`);
        skipped.push({ number: prNum, title: pr.title, reason: err.message });
      } finally {
        try { runCmd(`git branch -D ${branchName}`); } catch {}
      }
    } else {
      skipped.push({ number: prNum, title: pr.title, reason: 'No write permissions or git workspace available' });
    }
  }

  // Push local merges if any were performed via git
  const gitMergedCount = merged.filter((m) => m.method.startsWith('Git')).length;
  if (isGitRepo && gitMergedCount > 0) {
    console.log(`\nPushing ${gitMergedCount} merged commit(s) to origin main via Doris SSH credentials...`);
    try {
      runCmd('git push origin main');
      console.log('✓ Pushed successfully to origin main. GitHub will now automatically close and link all referenced issues.');
    } catch (pushErr) {
      console.error(`Failed to push merged commits to origin main: ${pushErr.message}`);
    }
  }

  console.log('\n========================================');
  console.log(`Auto-Merge Summary: ${merged.length} merged, ${skipped.length} skipped.`);
  console.log('========================================');
  for (const m of merged) {
    const issues = m.linkedIssues && m.linkedIssues.length > 0 ? ` (Linked: ${m.linkedIssues.map(i => '#' + i).join(', ')})` : '';
    console.log(` - PR #${m.number}: ${m.title} [${m.method}]${issues}`);
  }
  for (const s of skipped) {
    console.log(` - PR #${s.number} (Skipped): ${s.title}`);
  }
}

main().catch((err) => {
  console.error('Fatal error in auto-merge runner:', err);
  process.exit(1);
});
