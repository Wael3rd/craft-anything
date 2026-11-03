# Roadmap

The shared plan for reaching the product described in the README, for people and agents alike.
`docs/STATUS.md` says what works today; this file says what comes next, who owns it and what an
agent may pick up; [roadmap-history.md](roadmap-history.md) keeps what's finished. Rebuilt on
2026-11-02 from `main` at `2e4afc5`, with v0.3.0 the latest release.

## How to use this file

- Each item has an **id** (`R<milestone>.<n>`), an **owner**, a **status** and a list of
  **pending changes**. Pending changes are small, independently mergeable units. Ids are never
  reused or renumbered: commits, code comments and PRs quote them.
- **Claiming:** before you start, put `— claimed: <name>, branch <branch>` on the pending change
  and push your branch early (or open a draft PR) so others see it. Don't take a change someone
  else has claimed. `— in PR #n` marks one a PR already covers: review that PR rather than starting
  again. The full working rules are in `AGENTS.md` ("Working rules", mandatory). Items owned by
  **maintainers** are steered by the project maintainers: agents may take their *unclaimed pending
  changes*, but design decisions marked **decision** wait for a maintainer.
- **Finishing:** tick the change (`[x]`) where it is, add the commit or PR, update `docs/STATUS.md`
  if what works changed, and add a line to `log/devlog.md`. Maintainers move ticked lines to
  `roadmap-history.md` when a release ships.
- **New work:** add a `- [ ]` line under the item it belongs to; a new item takes the next free id.
- **Status:** `todo` · `in progress` · `blocked (<why>)` · `done`.
- Follow `AGENTS.md` (layering, never crash, everything is a tool, `platform::process` for every
  external program) and the architecture rules below.

## Architecture rules for new work

The project should be easy to fork and bend. New features land behind a seam, not inside a
`match`:

1. **Traits plus a registry, not enums.** Target kinds, checks, builders, asset providers and
   external engines are traits with a registry that holds built-in implementations. Adding one
   means adding a file and one registration line (as tools already work).
2. **Data over code for catalogues.** Stacks, game-engine build recipes, model ids and prices,
   and engine install pins live in typed tables (or TOML files embedded with `include_str!`), not
   in string matches or prompt text.
3. **Headless first; flows are tools.** Everything works without the GUI. Anything a person
   can start from the UI (rebuild, follow-ups, answering questions, engine installs, launching a
   result) is reachable through `Engine::call` and the CLI, so MCP, agents, scripts and other
   frontends get it for free. Orchestration, persistence and process control never live in
   `ui-egui` or `apps/*`.
4. **Versioned files.** Every file we write and read back (`craft.json`, `config.toml`,
   `project.toml`) carries a version, has a JSON schema, and has a migration path.
5. **Structured results.** Decisions such as "did the build pass" read typed fields, never
   English text prefixes.

## Milestones at a glance

| Milestone | Goal | Owner | Where it stands (2026-11-02) |
|---|---|---|---|
| M0 Foundations | Docs match the code, licences checked, one live end-to-end run | maintainers | Done |
| M1 Extensibility | Seams for target kinds, checks, builders, asset providers, engines, plugins | maintainers | Done, except engine pins (R1.5) and Unreal's Linux guidance (R1.6) |
| M2 Product completeness | Every kind of target gets study → build → check → refine with a real check | maintainers | Websites complete; apps and games build and run; API-key builds have never run live (R5.5); agents other than Claude Code are only partly wired (R2.2) |
| M3 Engine depth | More tools, and the known partial ones | open | 314 tools; the open lane |
| M4 Release | Packaging, signing, a managed tier | maintainers | v0.3.0 released, signed, on every platform; a macOS CLI question and the managed tier remain |
| M5 Works for real people | What people hit now that the project is public | maintainers | Comes first |

## Open work

Every unticked `- [ ]` line in this file is open work, under the item it belongs to.
`cargo xtask pending` lists them all, grouped by item (`--count` for the totals). When you find
work that's missing, add it as a `- [ ]` line under the right item in the same PR, rather than
leaving it in a PR description or a chat.

## Order and dependencies

Work in phases. Items in the same phase can run in parallel. A phase says what comes first; what
stops an item from starting is its **Depends on** column, which must be done first. A *soft*
dependency should come first but doesn't block.
Issues are ranked by people affected and how badly, after leaving out what's already fixed.

| Phase | Item | Depends on | Why here |
|---|---|---|---|
| 0 Ship | R5.1 The next release | — | The Windows start-up crash and a build-breaking resume bug are fixed on `main` but not released |
| 0 | R5.2 Keeping contributions moving | — | `main` isn't protected, first-time contributors' CI is held, and a waiting PR fixes a safety bug |
| 1 Builds that work | R5.3 A builder that can't make progress stops | — | Costs people money or quota on every backend; small |
| 1 | R5.4 Local models can build | R5.3 *(soft)* | Largest open gap, with no fix anywhere yet |
| 1 | R2.2 Every builder gets the same tools (that group), and Approvals | **decision** for Approvals | Agents other than Claude Code start but can't do the job, and Clean room's rule doesn't reach them |
| 1 | R5.5 API-key builds fail in words people can use | — | This path has never been run live, and its errors say nothing |
| 1 | R5.6 Tools report only what they saw | — | Confirmed false successes on macOS; golden rule 3 |
| 2 Proof | R5.7 Live checks owed | — | Built but never run for real; each can reopen work above. Lines run when the machine or key they need is at hand, Clean room and the quick start first |
| 3 Depth | The rest of R2.2 | — | Detection waits for reports from 0.3.0 or later; ACP after the adapters work |
| 3 | R2.4 Runtime study, R2.5 Behaviour checks | scripted input (R2.4) before R2.5's sheet check | Makes "ready" mean more for apps and games |
| 3 | R2.7, R2.8 | — | Small gaps left in finished work |
| 3 | R2.9 Everyday use, R2.10 Pinned builds | — | Open to anyone at any time; most lines stand alone |
| 3 | R1.5, R1.6 | — | Small leftovers |
| Any | M3: R3.3, R3.9, R3.11, R3.12 | — | Open lane |
| Later | R4.1 macOS CLI, R4.2 Managed tier | **decision** | Not before M5 |

---

## M5 Works for real people

### R5.1 The next release
Owner: maintainers · Status: todo
Two reported failures, the Windows start-up crash (#414) and a builder resuming another's
session (#471), are fixed on `main` and still broken for everyone on 0.3.0. This is the
cheapest help we can give anyone, so nothing below should hold it up for long.
- [ ] #490 has changes requested: a JSON object given to `--left` or `--right` becomes text, and
      its new test passes without the fix. Merge it once that's fixed, or decline it; it doesn't
      hold up the release. What it reports is 0.2.0's, fixed by #93 (`f6725c2`): on Windows the
      published 0.3.0 CLI with Claude Code 2.1.179 mounted 314 of 315 tools (the 315th is the
      approval tool, which Claude Code doesn't list but calls), and a real Deep build reached Ready
      with `core_action_approve` answering five prompts. The reporter was asked for `--version`.
      macOS is still owed (R5.7)
- [ ] Cut the next release (`docs/releasing.md`). 0.3.0 lacks `0748ef1` (#414: Windows starts DX12
      only, the crash behind "it won't open"), `1e8f661` (#471: a builder never continues another
      builder's session) and `3357daf` (#430: Claude Code's message on stdin). Take #357 (R5.2) too
      if it's ready
- [ ] See that the launcher offers the new version: #74, #104 and #420 installed through it, and
      nothing in this repository says how it picks one
- [ ] After it ships, close what it fixes: #166, #346, #420, #478. #74, #104 and #222 give no
      detail, so they may be another cause: close them too, saying to file again if it still fails.
      Ask #201 whether `3861132` (#379) ended its early compression (a guess at the link)
- [ ] Close the tickets 0.3.0 already fixed, and the spam: #47, #53, #66, #95, #112, #113, #130,
      #133, #138, #152, #179, #182, #185, #202, #214, #329, #344, #377, #378, #400, #449; spam #194,
      #195. #113, #329 and #377 are 0.2.0's missing CLI, fixed by `1b54cbf`, not what #490 reports.
      Not yet: #64 (its cause is fixed, its retries aren't: R5.3), #142 (R5.3), and #462, which says
      `none`, not #490's failure, and gives no version: ask for it
- [ ] Sometimes starting the app shows nothing: after the release, see from `desktop.log` whether it
      still happens. DX12-only may have ended it; a GOG Galaxy Vulkan overlay layer was seen loading
      into the app
- [ ] Add to `docs/releasing.md`: before publishing, a Deep build through Claude Code from the
      finished package, on a Mac and on Windows. Starting the CLI from the package (#91) doesn't
      show whether Claude Code accepts its tools
- [ ] Add to `docs/releasing.md`: after a release, close every ticket marked as waiting for it, so
      they don't stay open marked `fixed`

### R5.2 Keeping contributions moving
Owner: maintainers · Status: in progress
Review comes before new work while the queue is long, smallest and clean PRs first. A PR is merged
only when it's green and a maintainer has fully reviewed it, code as well as wording.
- [ ] Maintainers: protect `main` (PRs required, squash only); the only ruleset covers `release`
- [ ] Auto mode's delete check is a safety bug that shows up as Mac test failures. It splits a
      command on spaces, so `rm -rf "C:/x/Game Backup/y"` from a project at `C:/x/Game` goes ahead
      unasked, and it lower-cases paths, so on a Mac or Linux a delete inside a project with
      capitals in its path is refused; `auto_mode_keeps_going_but_never_does_the_dangerous_things`
      fails on a Mac for that. `real_binary_end_to_end_if_present` fails there too, expecting
      `/bin/ls` to be Linux — in PR #357
- [ ] **decision** First-time contributors' CI: GitHub holds their runs for approval, so their PRs
      show no `gate` run (#490 and #413 among them, and #413 holds up #455). Approve each push, or
      change the repository's approval setting
- [ ] Merge order the PR list doesn't show: #413 before #455; #193 before #482 and #486 (all change
      the language list); #491 before #492 (same file). #433 and #468 look redundant with #421 and
      #500: ask their authors
- [ ] **decision** Wide PRs with no roadmap line: #416 (project goals, 34 files, cites an R2.11 that
      doesn't exist; that id stays free for it) and #503 (two asset providers and spending controls,
      37 files, cites the finished R1.4). Decide whether we want them before more is built on them
- [ ] **decision** Replies don't reach people who file through the ticket relay (each such ticket
      says so), yet the bot asks them for details. About half the open issues came that way. Get
      replies back to them, or stop asking
- [ ] `a_helper_is_judged_by_whether_it_runs` (ai crate) sometimes fails when several crates'
      tests run at once and passes alone: it writes a script and runs it straight away, which a
      busy machine can refuse while another test's child still holds the file open
- [ ] Revisit the two advisory exceptions in `deny.toml`: `rsa` (no fix; we only verify with public
      keys) when rsa 0.10 lands, and unmaintained `zbus_names`/`zvariant` (Linux-only via keyring
      and accesskit) when those move on. `deny.toml`'s comments say this is tracked in R0.3: point
      them here
- [ ] Check whether the release workflows still run actions that target Node.js 20. None is named
      here any more: every job calls `storytold/craft-common-actions@main`, so a fix belongs there
- [ ] STATUS.md's "Known limits" still says the project feed, game-engine and Ghidra installs and
      launching exist only in the desktop app, which R1.6 ended: update it

### R5.3 A builder that can't make progress stops
Owner: open · Status: todo
Every round spends the person's money or quota. One that can't do anything shouldn't start.
- [ ] A build stops and says why after two builder turns in a row that changed nothing in `app/`,
      on every backend (#44, #110, #417, #481). The count is saved in the project, so it holds
      across runs: a run ends at 7 checks (`max_rounds`) but check numbers carry on, so #44's check
      28 and #417's 35+ look like several runs (a guess: Try again is the only restart found, and
      Auto mode doesn't start runs)
- [ ] A builder whose tools didn't start isn't resumed. `turn` (`rebuild/mod.rs`) tries 3 more times
      ("Picking up where it left off", #377) because `worth_retrying` doesn't know this error. Give
      it a typed signal, as `Conversation::session_lost` does, not one more word in that function's
      list. Say what failed and stop (pitfalls: never retry what can't succeed)
- [ ] `worth_retrying` decides on English words in a message (AGENTS.md: structured results). Move
      its other cases (sign-in, credit, a missing install) to the same typed signal
- [ ] Find what repeated #64's attempts and stop it: it logged 41, about 38 s apart, on 0.2.0, but
      `turn` allows 3 retries 30, 60 and 90 s apart, there too, so something else starts them
- [ ] A rate limit is waited for only on agents with a session: `turn` retries only when
      `agent.session_id()` is set, which API backends and OpenCode's plain adapter never set (read
      in the code, not run). So #148 didn't reach #142, which was OpenCode. Wait and send the turn
      again for them too, then close #142
- [ ] **decision** Whether a build gets a token or cost limit of its own, beyond rounds: #399 used
      9.4M tokens in 19 minutes of the first stage, then failed to compile

### R5.4 Local models can build
Owner: open · Status: todo
The built-in agent loop assumes a capable tool-calling model. Reports #24, #44, #110, #403, #417 and
#498 say otherwise (#44's backend is a guess; #116 may be the same), and no local model build has
been run live.
- [ ] A live local build through Ollama, small target, to Ready or to a clear stop, and note in
      STATUS.md which models it worked with. Do this first: it shows which of the lines below matter
- [ ] An assistant turn with no text sends `content: null` (`openai.rs`), which likely causes #403's
      `invalid message content type: <nil>`: send an empty string
- [ ] Understand a tool call a model writes as JSON in its text (#417). The fallback protocol reads
      only `Action:` and `Action Input:` lines, so anything else ends the turn with nothing done
- [ ] Accept a tool argument of a near type, such as a list for a string, or answer so the model
      can correct it (#498)
- [ ] Find how each local server is told its context size and set it, or stop with a plain message
      when the prompt won't fit. We assume 24k and send no `num_ctx`, which an OpenAI-style endpoint
      may ignore anyway; LM Studio refused a 12.6k prompt on its 8k default (#73)
- [ ] A per-backend context budget in `[[ai.backends]]` (`LoopConfig::context_tokens` exists), for
      services whose real window differs from 150k (#374)
- [ ] Find LM Studio on another port (#73)
- [ ] See why Bionic or LM Studio can't be picked (#140)

### R5.5 API-key builds fail in words people can use
Owner: open · Status: todo
- [ ] Show the provider's own reason: #369 bought $10 of credit and got only "OpenRouter answered
      400: Provider returned error". Find the reason in the response body and show it
- [ ] A live API-key build (needs a key on the test machine)
- [ ] Failure logs for the other ways an AI stops (API backends, MCP server start-up), like the
      ones agent CLIs write to `logs/` in the config folder

### R5.6 Tools report only what they saw
Owner: open · Status: todo
Golden rule 3: the result is evidence. All four tickets are confirmed in the code, with no PR yet.
Each fix needs a Mac to test on.
- [ ] macOS: a denied Accessibility permission is reported as success with an empty tree (#240)
- [ ] macOS: `runtime_ui_read` ignores `screenshot` (#248) and `accessibility` (#307): the Swift
      helper declares both and reads neither
- [ ] macOS: call observation writes a literal `\n` into its lldb callback, so the Python is
      invalid and it reports zero calls (#251)

### R5.7 Live checks owed
Owner: open · Status: todo
The code for each of these is done; what's left is to run it for real and record the result in
STATUS.md. A failure becomes a new line under its item. Grouped by what the run needs. Start with
the Clean room build and the README quick start: Clean room is the default and has no live build
yet, and the quick start is how every new person begins.

**A signed-in coding agent:**
- [ ] A live Clean room build of a website and of an app, checked end to end
- [ ] A live build in a picked folder each way (edit in place, start from), checked end to end
- [ ] Thresholds under pressure: a clone that's off by a few pixels (a page with fluid type or
      `clamp()` spacing) to see whether 4 / 2 / 8 px is too strict
- [ ] Screenshot check: in a live run the final check once got no picture from an Electron app
      within 90 s while the same check passed in the builder's turn and on a re-run; not
      reproduced. The app waited on animation frames, which a window behind others may not get;
      the Electron steps now say to capture after loading instead
- [ ] A live chat run with a big comparison result, to see Claude Code page it with
      `core_evidence_browse` (and whether 80 KB is the right size)
- [ ] Live check on Windows: rebuild a project while its frozen pin runs, and the build isn't
      blocked

**An API key or a bill:**
- [ ] Run Nano Banana Pro once with a real key (a picture, then a 3D model from it) and check the
      picture's size and the invoice ($0.15)
- [ ] fal bills pictures "by rounding up to the nearest megapixel"; estimates use the exact size.
      Confirm on a real invoice whether a 1024×1024 picture bills as 1 or 2 megapixels, then round
      in the table (needs a key)
- [ ] A live extraction run with Kimi K3 (needs the person's OpenRouter key)

**A second computer or another system:**
- [ ] A live run of a project copied between two real computers with Claude Code (#451, #452); done
      on one Mac with a second `CLAUDE_CONFIG_DIR` standing in for the other computer (STATUS.md)
- [ ] Ghidra setup tried for real on Linux and macOS (the Java pins were checked by tests only)
- [ ] Follow the README quick start on a clean machine (Windows, and Linux or macOS)
- [ ] Build the new DMG on a Mac (release dry run) and look at the window once
- [ ] #490 on macOS: a real Deep build through Claude Code with the published 0.3.0 CLI, with
      2.1.179 and a current version. Windows passed; the Mac CLI was only checked to contain the
      fixed schema
- [ ] macOS CLI zip blocked by Gatekeeper when downloaded in a browser (#94), though Apple has its
      notarization tickets: find why the online lookup fails for people (it passes on our Mac, which
      may be exempt), on a machine that shows it

**A real program, device or file:**
- [ ] Cutter sessions: try them against a real Cutter, including whether `=h&` keeps Cutter
      responsive (only a stand-in server so far)
- [ ] Vineflower: run it once on a real JAR (no Java on the machine it was written on)
- [ ] Verify the 23 unverified adb tools; record results in tool-status.md
- [ ] Android locales: try `only_locales` on a real APK (only the test fixture so far)
- [ ] `app_graphs_compare` on the two real Electron releases again, to count how many of the
      162 "removed" now pair (and pair the `index-<hash>` chunks by structure if many stay)
- [ ] A real IL2CPP `global-metadata.dat` (needs a Unity build or a shipped game); synthetic so far.
      Compare a real APK's components against another tool's reading
- [ ] Stand-in backend: a browser-made HAR and a real front end talking to it (only a hand-written
      HAR so far); GraphQL and tRPC on real bundles (only hand-written minified shapes)
- [ ] Check the Kimi Code, Factory Droid and WorkBuddy registrations in the real apps

---

## M1 Extensibility

### R1.5 External engine trait in L3
Owner: open · Status: in progress
- [ ] More pins once their hashes are checked against the publisher: Chromium (no published
      checksums found yet), Rizin for Linux (the tar.xz unpacks to ~800 MB, so it needs streaming
      extraction) and macOS (a .pkg)

### R1.6 Headless first
Owner: maintainers · Status: in progress
- [ ] Give Linux-specific Clang guidance in Unreal's guided installer when the bundled compiler
      is missing (the requirements summary is already platform-specific).

---

## M2 Product completeness

### R2.2 More coding agents
Owner: open · Status: in progress
"Found on this computer" has to mean "can do the job". Today only Claude Code gets everything.
**Every builder gets the same tools:**
- [ ] A table of what each adapter gets (our tool server, approvals, resume, notes mid-turn, a
      message of any length), as typed data, with a test that fails when one is left out.
      `AgentAdapter` already answers some, such as `resumes` and `prompt_on_stdin`. The lines below
      fill it in
- [ ] OpenCode's plain adapter never passes our tool server, so its builder has none of our tools
      and hunts for a setting that doesn't exist (#481). Hand it the server, or build over ACP by
      default once that passes
- [ ] Aider has no adapter, so the plain one hands it the prompt as a file name, and it can open a
      browser sign-in nobody agreed to (#72). Give it an adapter, or stop offering it
- [ ] Codex (#499): in a chat the tool server has no active target, tools turned on with
      `enable_toolset` can't be called, and `rebuild_start` is refused by the approval policy. The
      reporter attached a patch for chat continuity; look at that first
- [ ] Codex: resume support (`codex exec resume <id>`), verified on a real install
- [ ] Every adapter but Claude Code's still takes the message in argv (`prompt_on_stdin`), so a long
      one can pass Windows' command-line limit as Claude Code's did (#402); hand each its message on
      stdin or in a file where its CLI reads one
- [ ] A note sent during a build reaches the builder at its next step on other coding agents with
      tool hooks, and on the API-backend loop between tool calls (Claude Code only today)
- [ ] Session copies for other agents that keep sessions (ACP agents' `session/load`) (#451)
- [ ] See whether Hermes still fails on 0.3.0 (#460: the fix its `fixed` label names was already in
      0.3.0 when it was filed, and it gives no version)

**Approvals:**
- [ ] **decision** One approval behaviour across agents, and whether we want an auto-approve mode
      apart from Clean room and Deep (Auto mode only governs what's asked through us). Today it
      differs by agent: Claude Code asks our approval tool; OpenCode's builder runs with `--auto`;
      Hermes's `-z` turns its approvals off on every run, chats included. Clean room's approval
      rule doesn't reach an agent that never asks us
- [ ] Say on each adapter, as typed data, what it can be held to (asks us, its own sandbox,
      nothing), and have runs act on that, once the decision above is made
- [ ] Check live what Hermes over ACP asks about: whether its file reads reach our approval policy
- [ ] Maki's plain `maki -p` builder doesn't ask us before commands, and in Clean room its reads
      aren't checked (as with every agent that never asks us): say so in the approval decision
      above, or run Maki over ACP by default once that passes. Outside a build, tools the person
      allowed in Maki's own permissions (an MCP server's, or all of them with `always_yolo`) still
      run; the adapter can't see that config
- [ ] Maki over ACP writes inside its working folder without a permission request (its built-in
      rule), and that folder is a build's project folder, so writes to our own records (inbox,
      manifest) skip the approval policy; `maki acp` takes no tool flags to stop it (0.7.0). Start
      it in `app/` (paths in the prompt relative to that), or ask Maki for a way to make those
      writes ask; part of the approval decision above

**ACP:**
- [ ] Gemini CLI, opencode, goose via ACP or native adapters; each verified live, then turn ACP on
      by default for the ones that pass
- [ ] Over ACP, a continued session shows the earlier conversation again: `session/load` replays it
      as `session/update`s, which we show as new; drop updates until the load answers
- [ ] Over ACP, turns report 0 tokens: the protocol sends only `usage_update` (context used and
      size). Read real counts where an agent keeps them (Maki: `token_usage` in its session file
      under `~/.local/state/maki/sessions/`) or say the count is unknown instead of 0

**Finding them:**
- [ ] #105, #135, #259, #385 and #423 predate 0.3.0, which looks in more places, and came through
      the ticket relay, so nobody can ask them for `doctor` output. Close them pointing at 0.3.0,
      and add search paths only for a new report that has that output
- [ ] Say plainly when what's found is the Claude desktop app rather than the `claude` program
      (#259, a guess at its cause)
- [ ] macOS: `/Applications/Antigravity.app` ended up as the agent's command (#467); find what
      writes it (no code for it was found; maybe the file picker)
- [ ] Windows: look in `%LOCALAPPDATA%\agy\bin` too (#62)
- [ ] "Claude Code on <ssh host>" is offered for any SSH host, whether or not it has Claude Code
      (#62)
- [ ] Gemini CLI's sign-in reportedly refuses individuals now (#400): check, and stop recommending
      it in the builder setup dialog if so

### R2.4 Runtime study for apps and games (with consent)
Owner: maintainers · Status: in progress
- [ ] UI tree on Linux (AT-SPI)
- [ ] Window capture tried on macOS and Linux (only Windows so far); games behind a launcher
- [ ] Short scripted input sessions (enigo) to record screens of the original

### R2.5 Behaviour checks for non-web builds
Owner: open · Status: in progress
- [ ] Behaviour checks that use the sheet: run the build with scripted input and measure values
      (speed, jump height, fire rate) against it (needs R2.4's scripted input)
- [ ] Hold the self-test to what the check can see too: picture the game after its scripted
      input and require it to differ from the `--screenshot` picture (a report alone is the
      build's own word)

### R2.7 The request decides how to build
Owner: open · Status: in progress
- [ ] Make: a game whose request names a technology the stack table doesn't know (Phaser, Bevy…)
      still needs the default engine installed to start, though the builder is told to use what
      was named; ask the person, or let such a game start without an engine
- [ ] The website check's "No built site found in app/dist, app/out or app/build yet" leaves out
      plain HTML in `app/`, which `site_dir` accepts: name it too (#494)

### R2.8 Clean room means clean room
Owner: maintainers · Status: in progress
- [ ] A remote (SSH) builder needs a craft-anything-cli that knows `--deep` and `--clean-room`:
      check its version first and say so when it's older

### R2.9 Everyday use: typing, projects, settings
Owner: maintainers · Status: in progress
Gaps found while using the app for real projects.
- [ ] A prompt brought back with Up keeps its pictures (only the text comes back now) — in PR #216
- [ ] Change a project's builder from its page (a tool, so the CLI and MCP can too) — in PR #245
- [ ] When the builder decides a question, show the question and its choice as an answered card
      in the feed (the run sees only the tool call today; the builder's words say what it chose)
- [ ] A question's own cost: a forked Claude Code session reports the whole session's running
      total, so a forked answer shows none; count questions in the project's usage
- [ ] "Make this change" on an answer that suggests one (sends it as a normal message)
- [ ] **decision** A program started from the CLI (`project_play`) keeps the shell's output pipe
      open, so the command doesn't return until it closes (#207). Every fix found needs `unsafe` or
      an unstable API: an exception to rule 9, or a limit we document. Meanwhile a reproducing test
      marked `#[ignore]` and a line in the CLI docs are welcome
- [ ] macOS: the window draws Windows-style controls at the top right, since the app turns off the
      system's title bar (`with_decorations(false)`), and its icon is bigger than the system's (#20)
- [ ] Show a command's output while it runs: Claude Code reports a tool's result only when it
      ends, so a long `cargo build` is one quiet line until then. Watching the agent's child
      processes from outside could say what's running and for how long
- [ ] The keep-awake helper once outlived a CLI `rebuild` on Windows by more than 9 minutes and held
      the caller's output pipe; not reproduced in 8 more runs, where it ended with the CLI. Dropping
      its guard only raises a flag (`RunningProcess::drop` waits only for `hands_off` programs), so
      ending on time rests on the Job Object closing, and its time limit is 7 days
- [ ] Find why a site capture lost its browser ("the WebSocket is closed", 419 ms into
      `web_site_capture` for a second page, seen once; the same capture passed 41 s earlier)
- [ ] Claude Code's own extras: its Ultracode switch (a setting, not an effort level; the
      effort environment variable we use can't turn it on) and its model names beyond the list
      (`opusplan`, `[1m]` variants) offered in Settings → AI; `ai model` already takes any name

### R2.10 Pinned builds
Owner: maintainers · Status: in progress
Keep a build that works at hand: pin it, and open it from the sidebar whatever the builder does
to the project next.
- [ ] Say when a light pin ended up copying what it meant to share (a drive without hard links)
      — in PR #220
- [ ] Pins of one project sharing with each other, so a third frozen copy of the same assets
      takes no more space
- [ ] Pin a website (a frozen copy of the built site, each served on its own port)
- [ ] A pin started before the app restarted shows as running (keep its process id and start
      time in the pin, and check both), and can be stopped
- [ ] Cap a run's log (`last-run.log`, `play.log`): it grows as long as the program prints
      — in PR #218
- [ ] Opening a pin that's already running brings its window to the front, instead of a message
- [ ] How much space pins take in Settings

---

## M3 Engine depth

An open lane: anyone may take these at any time.

### R3.3 Frida sessions for real
Owner: open · Status: todo
- [ ] Replace the in-memory session bookkeeping with real attach, resume and detach

### R3.9 Game assets out
Owner: open · Status: in progress
- [ ] Unity: sounds out (the stored stream, usually FSB5; PCM ones as WAV)
- [ ] Unity: crunched textures (DXT1Crunched, DXT5Crunched; two of the games checked use them) and
      the float formats (RHalf, RGHalf, RGBAHalf, RFloat, RGFloat, RGBAFloat)
- [ ] Unreal 5 and Oodle: IoStore packages, unversioned properties and Oodle-compressed paks
- [ ] Game files: save pictures from IoStore containers (needs the Unreal 5 support above)
- [ ] Unreal: sounds and meshes out (so a Deep-mode rebuild needs no outside converter for them)

### R3.11 Single-file apps, game files and stand-in backends
Owner: open · Status: in progress
- [ ] Stand-in backend: writes that change later reads (recorded answers are replayed as they are)
- [ ] Tell the builder about the stand-in backend in a website's brief, and start one from a
      recording
- [ ] Stand-in backend: a 304 recorded with no body is replayed as a bare 304, which a fresh browser
      can't use; say so or leave it out
- [ ] Godot format 4 and IL2CPP metadata 38 and 39 are header-only; read their tables once a real
      file can be checked
- [ ] Bun: take source maps and bytecode out as files; a standalone `.pyz`
- [ ] `native_source_folder_write` uses the built-in decompiler only: use Ghidra in Deep mode when
      it's there
- [ ] `native_publisher_show` hashes the file minus the signature fields, not section by section;
      a file with unusual gaps between sections can read as different

### R3.12 Console games in Deep mode
Owner: open · Status: in progress
- [x] Deep mode reaches processors the built-in engine can't decode (found on a Dreamcast game's
      `1ST_READ.BIN`, SH-4): a raw binary opens with any architecture name when a Ghidra
      `language_id` is given, `native_function_pseudocode` takes an address straight to Ghidra when
      the built-in decoder can't read the program (it used to stop before asking), and raw code is
      loaded into Ghidra where it runs (`-loader BinaryLoader -loader-baseAddr`, part of the cache key)
- [x] `native_functions_decompile` asks Ghidra in Deep mode, item by item, as
      `native_function_pseudocode` does (it used the built-in lifter only)
- [ ] String tools (`native_strings_find`, `native_strings_browse`) refuse a program the built-in
      decoder can't read (a MIPS PSP module), though finding strings needs no decoding
- [x] A PSP module (`.prx`, MIPS ELF of type 0xFFA0, or a PSP executable) is named for what it is:
      triage says "PlayStation Portable module" with its module name and version, operating system
      "PSP" and architecture `mips` (the architecture enum now has `Mips`, so no MIPS file reads
      "other"); `crates/formats/src/psp.rs` reads the module info, the import stubs by library and
      the export entries into `imports` (`<Library>_<NID>`, with a typed `nid` and the stub as the
      slot), `exports`, `libraries` and `details.psp_module`; corrupt tables give warnings
- [ ] PSP imports and exports have no names, only NIDs: a name table (built by us, from names we
      hash ourselves, not copied from another project) would let `<Library>_<NID>` read as a function
      name; imported variables are only counted (`details.psp_module`): where their NIDs are kept
      isn't read yet
- [x] Triage recognises `PARAM.SFO` (new target kind `psp-param-sfo`, `formats::psf`: title, disc id, category,
      firmware, every entry as a field, warnings for bad offsets) and an encrypted PSP executable (`~PSP`,
      kind `psp-encrypted`: module name from the header, says it must be decrypted first; native tools give
      that reason)
- [ ] Nothing decrypts a `~PSP` executable, so `EBOOT.BIN` and `.PRX` files in a game's `SYSDIR` have no
      code to read until they are decrypted elsewhere; the `PSAR` update archive (`DATA.BIN`, magic
      `PSAR`) isn't recognised either
- [ ] `PARAM.SFO` is read only when it's the target itself: the game folder and ISO inventories
      (`core_archive_show`) don't show its title or disc id yet
- [ ] Disc images: triage doesn't recognise an ISO 9660 image, a `.cue` or its `.bin` tracks
      (`core_archive_show` calls a PSP ISO "text"); `firmware_regions_list` finds the ISO 9660, but
      nothing extracts its files
- [ ] The managed Java doesn't start from a deep folder (its `jvm.dll` path past ~250 characters):
      Ghidra's launcher then blames `JAVA_HOME`, and our error points at an analysis log that was
      never written
- [ ] Ghidra's raw loader is given no entry point, so a raw program has no functions until one is
      asked for (each is made on demand)
- [ ] `deep::prepare` (the builder's analysis ahead of time) builds its cache key without a target's
      own `language_id` or raw load address, so for raw code (or a target opened with its own
      language) it analyses separately, loading the code at 0, and the builder's tools never reuse
      it (found in review); pass the target's settings through

---

## M4 Release

### R4.1 A macOS CLI that opens
Owner: maintainers · Status: todo
- [ ] **decision** A standalone macOS CLI that can carry its ticket, such as a stapled `.pkg`; that
      needs a Developer ID Installer certificate in the release secrets

### R4.2 Managed tier
Owner: maintainers · Status: todo
- [ ] **decision** Managed tier: account backend, usage metering (Claude API under our own key only)
- [ ] Before the managed tier or any paid offering: confirm the Claude Code orchestration question
      with Anthropic (claude-code.md)

---

## Parked

Looked at and not committed to; none of this is open work. Say in the issue if you want one.

- From the tool survey: firmware image kinds (ESP32, U-Boot, device trees, Intel HEX), Erlang BEAM,
  GPU code objects, kernel modules, GameCube DOL, Mac crash reports, SAZ and qlog captures, protobuf
  bodies, R8 mappings, JNI links, MCP over HTTP
- Ideas from issues: resume by itself after a usage limit (#476, the one tied to a real problem), a
  source folder as a target (#480), actions after a build ends (#495), WebAssembly disassembly
  (#175), an HTTPS intercepting proxy (#164), VM and Windows Sandbox detonation (#144, #163), an
  adapter for another disassembler (#147)
- More providers and agents (#14, #63, #97, #162, #170, #186, #206, #470): R2.2's table comes first,
  so each new one is a small addition rather than another half-wired agent
