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
  post-award input step. **The pinned contribution base must be publicly readable too** — see
  [Contribution bases](#contribution-bases) below; a post that violates this is refused before
  any money or upload work.
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
and its reachable history into the per-job repository **before either a targeted or open-pool
private offer is published**. If preparation fails, the offer is not published. This means the
seller's delivery push can reuse the base already on Maxplayer instead of uploading it again.
Source credentials, Git hooks and Git config are not copied into execution. A follow-up is a new
offer with explicit input and history pins, not an amendment to the old task.

### Contribution bases

Who must be able to read `target_repo_url` depends on the job shape:

| Job shape | Who reads the base | Private (unreadable) base |
|---|---|---|
| Targeted private (`seller_pubkey`) | The buyer imports it; the chosen seller reads the per-job repo | **Works** — use `base_local_path` for a local private source |
| Open-pool private (`untargeted`) | Every prospective seller, before it claims | **Refused at post time** |
| Public (targeted or open) | The executing seller, from the source URL | **Refused at post time** |

The refusal is deliberate and fail-closed: an open-pool or public job whose base sellers cannot
read would never get a working claim, so the post is refused **before the wallet opens and before
any upload**. The refusal names the two working alternatives (make the repository public, or post
a targeted private job). Two checks implement it:

- A per-job private repository URL (`<git_base><buyer>/<job>`) is refused statically.
- A non-relay source is probed and fetched exactly the way a bidding seller reads it:
  anonymously. maxplayer never sends your Git credentials, so a private GitHub repository fails
  this fetch and the post is refused with the documented message.

A **targeted private job with a private repository is the supported combination**: the buyer
uploads the pinned base into the per-job repository, and only the chosen seller (and the service)
can read it.

### Slow preparation and offer re-signing

The base import and upload can take long on large repositories. The relay refuses any event whose
timestamp is more than ±15 minutes from server time, so an offer signed before a long upload could
become permanently unpublishable. Two mechanisms close that hole:

- When preparation takes more than 10 minutes, the buyer **re-signs the offer** with a fresh
  timestamp just before publication and moves the per-job repository binding to the new offer id
  (the uploads stay valid; only the job's public id changes). A relay without re-bind support
  refuses the move and the buyer falls back to the original offer.
- If the relay still refuses publication permanently (`invalid: …`), the refusal is **recorded
  and surfaced** instead of retried forever: `post_job`/`get_job` return `publish_refused` with
  the relay's reason, the auto-award parks with that reason (visible in `maxplayer buyer status`),
  and the recovery is to post the job again.

The buyer and configured service can read the prepared repo. A targeted seller retains pre-claim
access; open-pool bidders do not get that access. They still check the publicly identified source
before bidding and cache the exact base for execution. Only the selected open-pool seller gains
job-repo access at award. This does not add confidential attachments or privately accessible
source discovery to open-pool jobs. Buyer input refs are immutable and become frozen when the
offer is published (or awarded); later provisioning cannot reopen them.

Buyer preparation transfers have a size-scaled HTTP request ceiling: 300 seconds minimum, one
second per 128 KiB of pack, capped at 30 minutes (the relay's receive window and the fronting
proxy allow that), and a 15-second connection limit. Input uploads retry transient failures up to three attempts, with 1s then 2s backoff and
fresh authorization per request. An authenticated read of the exact input ref recovers an upload
whose success response was lost; a different commit at that ref is refused. Authorization failures
stop immediately. Existing object and size quotas still apply. These are per-request ceilings,
not unlimited/progress-based uploads or a whole-operation deadline. Payment-time verification
keeps its existing shorter limits.

Deploy the updated relay before upgrading buyers: older relays reject open-pool pre-publication
provisioning and uploads. Existing offers/seller fetch behavior remain compatible; this change
does not retroactively preload jobs already published.

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
