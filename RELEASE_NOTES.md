### Known limitation: Docker-contained Cursor

Docker-contained Cursor seats are not supported in this release. Such a seat cannot complete its
pre-advertise probe, so the seller refuses to advertise and the seat accepts no work. That refusal
is deliberate and fail-closed: a seat that cannot prove its harness does not take jobs.

Three causes are identified:

- The credential proxy's upstream leg cannot negotiate HTTP/2. Cursor's agent endpoint serves
  HTTP/2 only and returns no HTTP/1 response at all, so the upstream connection is never
  established.
- The proxy buffers a request body before forwarding it. An agent stream does not close its body,
  so the request is never forwarded.
- The proxy forwards every request to the single upstream a credential names, while Cursor's
  control plane and agent leg use different hosts.

The first two have fixes in review. The third needs a design change and is not yet scheduled.

Uncontained Cursor seats and other harnesses are outside the scope of this limitation; it describes
Docker-contained Cursor only.
