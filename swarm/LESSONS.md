# Lessons from running a large agent swarm

Raw notes from one long session with a coordinator and up to 20 agents at once. Generic lessons only. The operating rules are in `RULES.md` next to this file, the scripts are `integrate.sh`, `wt.sh`, `run-title.sh` and `watchdog.sh`.

## The run in numbers
- About four and a half hours, one coordinator session, up to 20 agents at once, 30 or so agents in total. Models mixed: a few of the most capable class at high effort for compiler and memory work, the rest one and two classes below.
- Cost: half of a weekly usage limit of the largest plan. About 7 % in the first 45 minutes, 50 % after about four hours. The owner stopped the full load at that point.
- Side products: a message bus for other contributors' agents with a dashboard, built and hosted the same evening. Two guests connected and took work.

## What worked
1. One lead per subsystem who writes the design and the work packages first, then packages in parallel. The lead found the whole work once, the packages implemented it. Package owners talk to the lead, not to the coordinator.
2. A trunk branch per subsystem, maintained by the lead. Before that, package branches drifted apart and conflicted in shared files (workspace manifest, module lists).
3. A census tool that counts, for the whole corpus of test inputs, what works and the first blocker of everything that does not. Every agent could see its own number move. The blocker list was the work queue.
4. A fixed message protocol: READY branch sha with how it was verified, FIX for a second problem the sender will not solve, STUCK, LANDED. Agents carry on after READY and hear back only on failure.
5. One integration agent that owns the lock, rebases, runs the full checks and pushes. Analysis never waits for build and tests.
6. One worktree per agent, a shared compiler cache, a cap on build jobs.
7. A rules file every agent reads once (RULES.md), so briefs stay short and rules change in one place.
8. Measuring before believing. Every wrong hypothesis of the evening (five or six) was killed by a run or a probe, usually within one run.
9. Delete what a change replaces in the same change. No second path next to the new one.
10. Agents that say "my session has grown too long, hand this to a fresh agent" with an exact list. One did. That handover was cleaner than pushing on.
11. Offers to guests instead of orders: TASK and RUN_REQUEST with ACCEPT or DECLINE. Guests bring other hardware and their own budgets.

## What went wrong, and the fix
1. Budget. Nobody watched the burn rate. The coordinator should have projected it after the first hour (7 % in 45 minutes means the week is gone in ten hours) and proposed a ceiling. Fix: state the burn rate with every status report, set a ceiling per day before starting.
2. Every agent message wakes the coordinator, with its whole context. Chatty progress reports were a large hidden cost. Fix: READY or STUCK only, progress goes to a file or a board.
3. Waiting agents are not free. Agents in poll loops (census on a timer, a comparer waiting for a self test) kept waking up. Fix: no timers inside agents, the coordinator or a script triggers them.
4. Machine load. With 15 agents building, a title run made 320 frames in 190 s. An hour earlier the same build made 2,528 in 158 s. Timing based findings from loaded runs were worthless. Fix: the scarce resource (one title, one GPU) sets the size of the swarm, not the number of ideas.
5. The serial resource was the real limit. One title run at a time, 2 to 4 minutes each, and the title shows its next problem only after the previous one is fixed. More agents did not shorten that chain.
6. Flaky tests under load blocked every landing three times. Each was a real test bug (a system wide counter, an extra handle, an address lookup that moved). Fix: a red main stops all landings, so the fix goes first and the coordinator may do a small one itself.
7. A script edited in place while another agent's run used it. Two runs lost. Fix: write a new file and move it over the old one.
8. Stopping queued background jobs did not kill their child processes. Orphans ran in random order. Fix: one queue owner, never many queued scripts.
9. Branches only on a private fork while guests could only see the shared repository. Guests were blocked without saying so for a while. Fix: decide where shared work lives before inviting anyone, and tell agents about remote changes (several agents reported the second push address as a suspected mistake, which was the right reflex).
10. Loose words in status reports. A vague label for a list made a guest's owner ask why a feature was missing. Fix: say what was seen, name lists by what they are.
11. Restarting a shared service without announcing it.
12. Guessing instead of measuring by the coordinator itself (a size limit, a wait time). Fix: the coordinator's guesses go to an agent as a question, never into a brief as a fact.

## Open questions for the retrospective
- How much of the cost was the coordinator, how much the seven top class agents, how much the rest. The session logs have token counts per agent.
- Whether the lead plus packages shape would have worked with cheaper models for the packages.
- What a good size is when one serial resource dominates. A guess: one agent on the serial chain, one or two feeding it, and everything else only if it does not load the machine.
- Which of the rules belong in a skill and which were specific to one project.
