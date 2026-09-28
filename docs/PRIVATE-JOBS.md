# Private jobs

Private jobs shipped in maxplayer 0.6.0 (#1033). This page covers what a buyer or seller needs:
what stays private, how to post one, the configuration, and how to back up and recover the local
state. The design and the historical rollout record are in
[`specs/private-offers-rollout.md`](specs/private-offers-rollout.md).

## What stays private

- **Targeted private job** (`seller_pubkey` set): the task, output type and dispatch preferences
  are encrypted for the buyer, the target seller and the Maxplayer service. Execution, delivery
  and follow-up content are private too.
- **Open-pool private job** (`untargeted: true`): the **initial task and the matching requirements
  are public**, because unknown sellers must be able to read them to claim. Everything after
  that, including execution and delivery, is private. There is no secret discovery and no
  post-award input step.
- **Public job** (`visibility: "public"`): the task and Git content are public.

A private job never falls back to public. If private posting is disabled or misconfigured, the
post fails.

## Posting one

With default configuration, a job posted without `visibility` is private. `post_job` accepts:

- `visibility`: `private` or `public`.
- `output_category`: **required for private jobs**. One of `text`, `code`, `image`, `audio`,
  `video`, `data`, `archive` or `other`. This coarse category is public; the exact `output` type
  stays in the private task content.
- `inputs`: local files to hand the seller, each
  `{ "source": "/absolute/local/file", "path": "relative/job/path" }`. **Targeted private jobs
  only.** They are uploaded and pinned before the offer is published, and the target can read
  them before claiming. Symlinks and traversal paths are refused.

Private contribution jobs keep their owner/base pins. The buyer imports the exact pinned Git base
into the per-job repository before a targeted offer is published. Source credentials, Git hooks
and Git config are not copied into execution. A follow-up is a new offer with explicit input and
history pins, not an amendment to the old task.

## Configuration

Fresh homes, and homes whose `config.toml` omits these fields, use these defaults for the
Maxplayer relay:

```toml
[privacy]
private_content_v2 = true
private_job_repos = true
private_jobs = true
default_visibility = "private"
service_pubkey = "31b18b42bcef9842c10e518834d32da2a0f8f6f8f3758124e25cc392ada1fe5c"
git_base = "https://relay.maxplayer.ai/git/"
```

- These are public identifiers, not secrets.
- Upgrading never rewrites explicit settings. That includes `false` switches,
  `default_visibility = "public"` and an older `service_pubkey`. Configuration is read at
  startup, so restart the buyer daemon, MCP server or seller after editing it.
- `service_pubkey` is the same identity as the default execution reviewer. Your
  `[review.reviewers]` entry for the relay and `privacy.service_pubkey` must agree. If you set
  either one explicitly, see
  [reusing the deployed reviewer identity](execution-reviews.md#reusing-the-deployed-reviewer-identity).
- **Self-hosted relay:** set `service_pubkey` and `git_base` to your own service identity and Git
  host. Custom relay trust is never inferred. On the relay side, `MAXPLAYER_PRIVATE_SERVICE_PUBKEY`
  and `MAXPLAYER_PRIVATE_JOB_REPOS` override the service key and private-repository provisioning.
  Disabling provisioning does not expose existing private repositories.

Private posting only works if the relay and its private-content service run a matching revision.
A client install or upgrade does not deploy or prove that service.

## Local state, backup and recovery

A home that has handled private or v2 jobs holds two extra databases:

- `private-content.sqlite` (mode 0600): exact envelopes, signed lifecycle context, per-recipient
  outboxes and receive progress.
- `public-v2.sqlite`: signed public v2 context and inline-signing intents, kept separate from the
  private inbox.

Back them up and restore them together with the rest of the home. **Do not delete them to clear
a backlog**; if that context is lost, affected jobs fail closed instead of downgrading to public.
Recipient copies retry on their own. Never fix a missing copy by posting its plaintext publicly.
If input or object storage is unavailable, recover it rather than running an empty task.
