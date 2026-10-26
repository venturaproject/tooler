# AI Factory: issue to reviewed pull request

Tooler's AI Factory is an opt-in, human-supervised workflow for taking a small GitHub issue
from specification through implementation and checks to a pull request. The playbook is the
user-facing entry point; Tooler's Rust runtime owns local state, safety gates, Git operations,
and agent process adapters. Python is not required.

The pilot is deliberately conservative. It does not merge pull requests, deploy, access
production, or run in the background. A person remains responsible for the issue, specification,
review and merge.

## Requirements and eligibility

Install/configure the tools needed for the stages you intend to run:

- `tooler` (built with the AI Factory command), `git`, and GitHub CLI `gh` authenticated for
  the repository.
- A Git checkout with an `origin` remote and a checked-out branch. AI Factory creates a new
  branch from the GitHub repository's default branch; it does not use uncommitted changes in
  the invoking checkout.
- At least one supported agent executable: `opencode`, `claude`, or a custom adapter wrapper.
- Rust toolchain (`cargo fmt`, Clippy and tests) for the checks stage.

The GitHub issue must carry both `ai-ready` and `ai-low-risk`. The following labels also stop
the run when present: `security`, `database`, `migration`, `production`, and `high-risk`.
These labels are only an initial gate: the spec agent must also classify the work as
unambiguously low-risk, and humans should stop if its scope is unclear or disallowed.

Out of scope for this pilot: security/authentication work, migrations, schema or data changes,
production access, secrets, CI/workflow changes, and business-critical behavior. Do not use
this workflow for issues that touch these areas, even if their labels appear eligible.

## Workflow and gates

The playbook at [`../playbooks/ai-factory.yml`](../playbooks/ai-factory.yml) runs these stages
in order:

1. **Prepare** — validate issue labels, capture issue metadata, check for an existing PR, and
   create a dedicated Git worktree and issue branch from the remote default branch.
2. **Spec** — ask a read-only agent to inspect the worktree and produce `SPEC.md`. The spec
   must explicitly start with `SPEC_STATUS: READY` and `RISK_CLASSIFICATION: LOW`; otherwise
   the workflow stops.
3. **Approve** — inspect the printed spec path and type `APPROVE <12-character-hash>` in an
   interactive terminal. Approval is bound to the complete SHA-256 of that exact spec.
4. **Implement** — run the selected builder from the isolated worktree. It may edit files but
   must not commit, push or create a PR.
5. **Checks** — run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
   `cargo test` in the worktree.
6. **Review** — request a read-only review using a different agent adapter or model from the
   builder. A result other than `REVIEW_RESULT: NO_FINDINGS` blocks progression; fix findings,
   rerun checks, and request a new review.
7. **Create PR** — verify the issue and approval are unchanged, check the same content snapshot
   passed both checks and review, validate every changed path, stage only the explicit safe
   path list, commit, push, and open a PR. It never merges or deploys.

Changing the spec invalidates its approval and later stages. Changing implementation files
after checks or review invalidates the corresponding evidence. Local state records the spec
hash, approval, worktree fingerprints, review result, and created commit/PR to prevent an
out-of-order run from skipping gates.

## Agent adapters

The playbook selects an agent and model independently for specification, implementation, and
review with `spec_agent`/`spec_model`, `build_agent`/`build_model`, and
`review_agent`/`review_model`.

### OpenCode

Select `opencode` and provide a model in `provider/model` format. Tooler invokes
`opencode run --standalone`, forcing a private server instead of connecting to a background
server that may be rooted in a different checkout. The process working directory is the
dedicated worktree.

### Claude Code

Select `claude` and optionally provide a Claude Code model name. Tooler invokes print mode,
disables session persistence and interactive permission prompts, enables restricted mode, and
allows only `Read` for specification/review or `Read,Edit,Write` for implementation. It does
not enable permission bypass flags. Tools not allowed by this configuration should fail rather
than prompt for approval in a non-interactive stage.

### Custom agent protocol (`stdin-v1`)

Set `TOOLER_AI_CUSTOM_ADAPTER` in the environment to the path of an executable wrapper. The
value is treated as one executable path, not a shell command; put any arguments in the wrapper
itself. Tooler runs it with the worktree as its current directory, writes the prompt to stdin,
captures its final response from stdout, and inherits stderr. It sets:

| Variable | Meaning |
| --- | --- |
| `TOOLER_AGENT_STAGE` | `spec`, `implement`, or `review` |
| `TOOLER_AGENT_MODEL` | Model value selected for this stage (may be empty) |
| `TOOLER_AGENT_WORKTREE` | Absolute worktree path |
| `TOOLER_AGENT_PROMPT_PROTOCOL` | Currently `stdin-v1` |

The wrapper must return a nonzero exit code on failure and print its final answer to stdout.
It is responsible for enforcing its provider's own tool restrictions. As with all adapters,
Tooler checks that read-only stages leave the worktree unchanged and that the invoking
checkout has not changed after the agent returns.

Example wrapper skeleton:

```sh
#!/bin/sh
set -eu
stage=${TOOLER_AGENT_STAGE:?}
model=${TOOLER_AGENT_MODEL:-}
worktree=${TOOLER_AGENT_WORKTREE:?}
test "$PWD" = "$worktree"

# Replace this with a provider command. Do not enable permission bypass for implementation.
exec my-agent --stage "$stage" --model "$model" --prompt-stdin
```

## Running the playbook

First check the playbook without executing tasks:

```sh
tooler play playbooks/ai-factory.yml --lint
tooler play playbooks/ai-factory.yml --dry --var issue=123 \
  --var spec_model=openai/MODEL_ID \
  --var build_model=openai/MODEL_ID \
  --var review_agent=claude --var review_model=CLAUDE_MODEL
```

Then run it with the desired adapters/models:

```sh
tooler play playbooks/ai-factory.yml --var issue=123 \
  --var spec_agent=opencode --var spec_model=openai/MODEL_ID \
  --var build_agent=opencode --var build_model=openai/MODEL_ID \
  --var review_agent=claude --var review_model=CLAUDE_MODEL
```

The dry run executes none of the commands and makes no GitHub or agent calls. `issue` is a
positive GitHub issue number. OpenCode requires a non-empty model. The default playbook uses
OpenCode for spec/build and Claude Code for review; override the variables for your setup.

For a deliberate pause after creating and inspecting the spec, run only the initial stages:

```sh
tooler play playbooks/ai-factory.yml --tags prepare,spec --var issue=123 \
  --var spec_agent=opencode --var spec_model=openai/MODEL_ID
```

Then resume with the remaining stages, preserving the same issue and stage variables:

```sh
tooler play playbooks/ai-factory.yml --tags approve,implement,checks,review,create-pr \
  --var issue=123 \
  --var build_agent=opencode --var build_model=openai/MODEL_ID \
  --var review_agent=claude --var review_model=CLAUDE_MODEL
```

Approval itself is an interactive hash challenge. Do not use `--yes` or other automation to
bypass it. If a task fails, inspect the worktree/artifacts before retrying; use the playbook's
`--resume` only for a failed invocation with a saved checkpoint. Never assume that skipping
playbook tasks by tags or `--start-at-task` creates the required internal stage records: each
`tooler factory` stage checks its own prerequisites and fails closed.

## Local files and recovery

Tooler stores local run metadata and prompts outside the checkout under a sibling directory:

```text
<checkout-parent>/.tooler-ai-factory-artifacts/<checkout>-<branch>-issue-<number>/
  run.json
  SPEC.md
  IMPLEMENTATION.md
  REVIEW.md
```

On Unix, directories are created with mode `0700` and files with mode `0600`. Issue text is
stored in local run metadata to support stage prompts. Treat this directory as sensitive,
inspect it before sharing, and do not commit it. The isolated worktree is a sibling under
`.tooler-ai-factory/`; its branch is named `tooler/ai-issue-<number>-from-<checkout-branch>`.

If preparation stops partway through, do not delete anything blindly. Inspect the Git worktree
list and the matching artifact directory. A partial setup requires manual inspection. If an
agent changed the invoking checkout, stop and inspect/recover those changes yourself; Tooler
does not revert user files. After the PR is handled, remove a clean worktree explicitly:

```sh
git worktree list
git worktree remove /path/to/.tooler-ai-factory/<worktree-name>
```

Keep the artifacts if you need the audit trail; otherwise remove only the issue-specific
artifact directory after checking its contents. Do not use force removal for a dirty worktree
until its changes have been reviewed and preserved.

## Safety boundaries and limitations

- A worktree isolates filesystem edits from the invoking checkout; it is not an OS sandbox.
  Agents and custom wrappers can still have network access and inherit environment variables.
  Run only trusted agents with appropriately limited credentials.
- The issue body is treated as untrusted prompt content. Prompt instructions are not a security
  boundary; human scope approval and the path/stage gates remain necessary.
- PR path checks reject sensitive directories/files, symlinks, oversized/binary files, and
  several common secret patterns. Pattern checks cannot prove that a file contains no secret;
  inspect the diff before merging.
- Rust formatting, Clippy and tests run locally. The workflow does not wait for GitHub Actions
  or verify required branch-protection checks before opening the PR.
- The review agent is advisory, not a substitute for human code review. A human must inspect
  and merge the PR.
- Tooler records local stage state but does not automatically clean up worktrees, artifacts,
  branches or remote PRs.
