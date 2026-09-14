# Seller tool config — templates and guide

This directory holds a template and a guide for a seller tool config. Use it to onboard a new
vendor tool into the holder.

The holder loads one seller tool config at startup. The config is the seller's offering. There is
no per-job config. The tool stays enrolled for the life of the seller daemon. See
[`../src/config.rs`](../src/config.rs) for the loader.

## Files

| File | Purpose |
| --- | --- |
| `seller-tool-config.template.json` | A skeleton to copy and fill. |
| `../fixtures/seller-tool-config.json` | A filled, runnable example (text transform). |

## Top-level fields

| Field | Type | Rule |
| --- | --- | --- |
| `seller_id` | string | The seller that owns the holder. Do not leave it empty. |
| `offering` | string | A one-line description. The seller declares it. |
| `vendor_base_url` | string | The vendor URL. Start it with `http://`. This kit ships no TLS. |
| `operations` | array | One entry per operation. The array must hold at least one entry. |

## Operation fields

| Field | Type | Rule |
| --- | --- | --- |
| `name` | string | The operation id. Use `[a-z0-9_-]` only. |
| `description` | string | Text for the `tools/list` schema. |
| `subcommand` | string | The vendor CLI subcommand. The holder fixes it. A job cannot select it. |
| `params` | array | One entry per parameter, in argv order. |
| `max_output_bytes` | integer | The output ceiling. The holder enforces it. Use a value above 0. |

## Parameter kinds

The `kind.type` field takes one of four values.

| `type` | Meaning | Argv result |
| --- | --- | --- |
| `job_input_file` | A file the job supplies, inside the job directory. | One flag and one path. |
| `job_output_file` | A file the holder creates, inside the job directory. | One flag and one path. |
| `choice` | One value from a fixed list in `choices`. | One flag and one value. |
| `text` | Bounded literal text, up to `max_len` bytes. | One flag and one value. |

Each parameter also carries a `flag` field, for example `--in`. The flag must start with `--`.

## Safety rules the holder enforces

The holder applies these rules to every call. You do not add them to the config.

1. The holder builds argv in the spec order. It never runs a shell.
2. The holder refuses an undeclared parameter. It does not drop it.
3. The holder refuses a `text` value that starts with `-`, or that holds a shell metacharacter.
4. The holder refuses a `choice` value that is not in the list.
5. The holder resolves a file path inside the job directory. It follows no symlink on any
   component. See [`../src/safeio.rs`](../src/safeio.rs).
6. The holder copies an input into a private staging directory. The vendor CLI reads the staged
   copy, not the job's own file.
7. The holder publishes an output with a no-follow create. It refuses a symlink at the output name.
8. The holder refuses the reserved names `job_id`, `job_root`, `cwd`, and `home`.

## How the seat uses this config

The seller daemon runs the holder for you. Build a holder image with `tool-holderd`, `holderctl`
and the vendor CLI on `PATH` (start `FROM` the kit image), then declare it in the seat's
`config.toml` under `[sandbox.held_tool]` with this config file and the credential file as absolute
host paths. See `docs/SELLER-QUICKSTART.md`, "Hold a vendor CLI for jobs", and the daemon side in
`crates/maxplayer-core/src/held_tool.rs`.

## How to onboard a new tool

Follow these steps.

1. Copy `seller-tool-config.template.json` to a new file.
2. Set `seller_id`, `offering`, and `vendor_base_url`.
3. Add one `operations` entry for each vendor CLI subcommand you want to offer.
4. Map each subcommand argument to one parameter. Give each parameter a `name`, a `flag`, and a
   `kind`.
5. Choose the `kind` for each parameter:
   - Use `job_input_file` for an input path.
   - Use `job_output_file` for an output path.
   - Use `choice` for a closed option set.
   - Use `text` for bounded free text.
6. Set `max_output_bytes` to the largest output you will return for one call.
7. Confirm the vendor CLI reads a credential from its own home. It must not need a credential on
   the command line.
8. Review the mapping against [`../../docs/specs/seller-tool-onboarding/03-command-policy-mapping.md`](../../docs/specs/seller-tool-onboarding/03-command-policy-mapping.md).
   A human with authority over the seller account does this review.

## How to test a config

Run the tests and the demo.

```bash
# Unit and integration tests.
cargo test -p maxplayer-tool-kit

# End-to-end Linux container demo. It needs a Linux Docker daemon.
cd crates/maxplayer-tool-kit
docker build -f docker/Dockerfile -t maxplayer-tool-kit:demo .
IMAGE=maxplayer-tool-kit:demo ./docker/demo.sh
```

The demo writes evidence to `evidence/<UTC-timestamp>/`. It prints a `PASS` or `FAIL` line for
each check.

## The runnable example

`../fixtures/seller-tool-config.json` offers one operation, `transform-file`. It maps to the
`transform` subcommand of the fake `vendor-cli`. It takes an input file, an output file, and a
`mode` choice of `upper`, `lower`, or `reverse`. The demo uses this config.

## An illustrative example

The block below shows a second shape. It maps a `text` parameter and a `choice` parameter. The
fake vendor does not support it, so it is an illustration, not a runnable config.

```json
{
  "seller_id": "seller-demo-02",
  "offering": "Render a titled document from a file the job supplies",
  "vendor_base_url": "http://vendor:8080",
  "operations": [
    {
      "name": "render-doc",
      "description": "Render a document and write the result into this job's directory.",
      "subcommand": "render",
      "params": [
        { "name": "input",  "flag": "--in",     "kind": { "type": "job_input_file" } },
        { "name": "output", "flag": "--out",    "kind": { "type": "job_output_file" } },
        { "name": "format", "flag": "--format", "kind": { "type": "choice", "choices": ["pdf", "png"] } },
        { "name": "title",  "flag": "--title",  "kind": { "type": "text", "max_len": 64 } }
      ],
      "max_output_bytes": 262144
    }
  ]
}
```

The `title` parameter maps to data. The vendor draws it into a header. Do not map a `text`
parameter to a value that the vendor reads as a path, a URL, or a config selector. See
[`../../docs/specs/seller-tool-onboarding/03-command-policy-mapping.md`](../../docs/specs/seller-tool-onboarding/03-command-policy-mapping.md).

## What a config cannot do

A config cannot do these things.

- It cannot add a command, an executable, or a shell string.
- It cannot set an environment variable.
- It cannot carry a credential, a credential path, or a credential selector.
- It cannot name a host path outside the job directory.

For the standing list of unsupported cases, see
[`../../docs/specs/seller-tool-onboarding/08-gaps-and-unsupported.md`](../../docs/specs/seller-tool-onboarding/08-gaps-and-unsupported.md).
