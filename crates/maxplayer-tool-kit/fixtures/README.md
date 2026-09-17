# Fixtures

`seller-tool-config.json` is the seller's offering: the operations this seller sells, the vendor
base URL, and the parameter grammar for each operation. It is **seller configuration** — there is
no per-job counterpart to any of it, and nothing in it is issued, metered or expired per job.

`vendor_base_url` here is a placeholder. Both the test suite and the Docker demo override it with
`--vendor-base-url`, because the fake vendor binds an ephemeral port.

## No credential file is committed here, deliberately

The vendor service and the holder both need a synthetic credential file. No such file is
committed; each is **written at run time**, and the two paths differ in a way worth stating
plainly rather than blurring:

- `docker/demo.sh` derives a **fresh random secret per run** on the host, writes it mode 0600
  outside the repository, and removes it on exit.
- `tests/common/mod.rs` writes a **fixed synthetic literal that lives in the test source**. The
  file is created at run time; the value is a constant. It is named for what it is and is not
  reproduced in this README, so grepping the fixtures directory for it finds nothing.

An earlier version of this file said only "generated at run time" and pointed at a
`scripts/demo.sh` that does not exist; the script is `docker/demo.sh`. Nothing
credential-shaped is committed either way, so no scanner has to decide whether one was real.

The credential is synthetic in the only sense that matters: the account it opens exists solely
inside `vendor-service`, a fake in this crate, and no live account, network egress or spend is
involved anywhere in this kit.
