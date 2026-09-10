# Caveat for this bundle

This bundle records a full demo run of the repaired kit: **39 checks, 0 failures**. It
demonstrates the F1, F2, and F3 repairs end to end in the container topology.

Read this caveat before you cite the `build` receipt in `manifest.json`.

## The image was built offline, not from the pinned digests

The Docker buildkit resolver on the run machine hung on the registry metadata for the two
digest-pinned base images. The host and the Docker VM both reached docker.io fast, and the pull
rate limit was not reached, so this was a wedged buildkit state. A Docker Desktop restart would
clear it, but that restart would stop other running containers, so it was not done.

To still verify the repaired binaries end to end, the image was built from the locally present
`rust:1-bookworm` tag, as a single stage, with no registry access. The compiled binaries carry
the F2 staging code.

## What this means for the receipt

- `built_image_id` is correct. It is the offline image that this run used.
- `build_base_image` and `runtime_base_image` describe the committed `docker/Dockerfile` pins.
  They do **not** describe the offline image. The offline image used `rust:1-bookworm`.

## What is still owed

Re-run the demo against the digest-pinned image from `docker/Dockerfile` once the Docker registry
resolution works again. That run re-earns the F4 reproducibility claim. This bundle stands for the
F1, F2, and F3 mechanism only.

```bash
cd crates/maxplayer-tool-kit
docker build -f docker/Dockerfile -t maxplayer-tool-kit:demo .
IMAGE=maxplayer-tool-kit:demo ./docker/demo.sh
```
