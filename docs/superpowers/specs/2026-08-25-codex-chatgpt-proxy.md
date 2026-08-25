# Codex ChatGPT Proxy Design

Status: version one is implemented. The host refresh step remains deferred.

## Goal

Let a Docker Codex seller use a ChatGPT subscription session.

Keep the real access token on the host.

Keep the refresh token on the host.

Keep all current Claude and API key paths unchanged.

## Scope

This first version reads a current Codex `auth.json` file before each Codex run.

It does not refresh an access token.

It does not mount the auth file into the container.

It does not put an access token in a Docker argument or environment variable.

It supports the built-in `codex` seller preset in Docker mode.

## Configuration

Add this optional table under `[sandbox]`:

```toml
[sandbox.codex_chatgpt]
auth_file = "/absolute/path/to/codex-home/auth.json"
```

The table enables the new mode only for a `codex-acp` command.

The `auth_file` path must be absolute.

The file must not give access to a group or another user.

An absent table keeps the current behavior.

## Host Session Read

Read `tokens.access_token` and `tokens.account_id` from the auth file.

Ignore all other fields.

Do not bind the refresh token to a Rust field.

Decode the access token payload and read its `exp` claim.

Require this remaining time:

```text
job timeout + 15 minutes
```

Refuse the run before Docker starts when the token has less time.

Read the file for each run.

This permits a later host refresh without a seller restart.

## Per-Job Proxy Route

Create two random placeholders for each Codex run.

Use one placeholder for the bearer token.

Use one placeholder for the ChatGPT account ID.

Configure Codex with a custom model provider.

Use these provider values:

```text
base_url = the per-job proxy URL
wire_api = responses
requires_openai_auth = false
Authorization = Bearer <access placeholder>
ChatGPT-Account-ID = <account placeholder>
```

Set `MODEL_PROVIDER=maxplayer-chatgpt` for `codex-acp`.

Put the same provider name in `CODEX_CONFIG`.

The configuration contains placeholders only.

Do not pass `OPENAI_API_KEY` or `OPENAI_BASE_URL` in this mode.

## Proxy Rules

Send the route only to this upstream:

```text
https://chatgpt.com/backend-api/codex
```

Permit these requests:

- `POST /responses`

- `POST /responses/compact`

- `GET /models`

Require both exact placeholder headers.

Replace both headers with the host values.

Keep the request body and URL unchanged.

Remove hop-by-hop headers.

Refuse all other methods and paths.

Replace both host values with placeholders in each response header and body.

Stop the proxy when the job ends.

This action revokes both placeholders.

## Compatibility

Do not change the current generic credential route.

Do not change the Claude OAuth route.

Do not change the OpenAI API key route.

Do not change the Cursor file route.

Do not change a host or launcher seller.

Do not activate the new mode for a non-Codex command.

## Controlled Seller Setup

Use a new `MAXPLAYER_HOME` for the Codex seller.

Use a dedicated Codex auth directory.

Use one seller slot.

Keep `claim_open_pool = false`.

Set `accept_offers_only_from` to the test buyer public key.

Use a dedicated Docker network and proxy port range.

Do not change the existing Claude seller home or process.

## Later Host Refresh

A later version will refresh the ChatGPT session on the host before a job.

The host helper will update the same auth file before the existing session reader runs.

The container will still receive placeholders only.

The container will never receive a refresh token.

The later version must keep request body replacement disabled.
