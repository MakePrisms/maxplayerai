# cashu-corpus — the spec and CDK source on disk

## Where

- `/opt/cashu/corpus/nuts/` — the Cashu NUTs, `cashubtc/nuts` @ `49a909ce4d0739824b3859d4b3da21e6c1abdaeb`
  (2026-08-23). One markdown file per NUT: `00.md`, `01.md`, … Read `README.md` first for the index
  of which NUT is which.
- `/opt/cashu/corpus/cdk/` — the full `cashubtc/cdk` source at tag `v0.17.2`
  (`6132607495ae0741e412a63f2acc34e4ccddfc55`). This is the authoritative answer to "what does CDK
  actually do", because it is the exact code the product links.
- `/opt/cashu/corpus/PINS.txt` — the two commits above, so I can quote them without guessing.

`.git` was removed from both at image build time. The corpus cannot be updated from inside a job,
and it does not drift underneath me mid-job.

## Grep discipline

The CDK tree is large. Searching it badly wastes a whole turn.

1. **Name the NUT first.** Protocol questions are answered by `/opt/cashu/corpus/nuts/NN.md`, which
   is short. Go there before the code.
2. **Then find the type, not the phrase.** `grep -rn "pub struct MeltQuote" /opt/cashu/corpus/cdk/crates`
   beats grepping for "melt quote".
3. **Scope to a crate.** `crates/cdk/src/wallet/` for wallet behaviour, `crates/cdk/src/mint/` for
   mint behaviour, `crates/cashu/src/nuts/` for the wire types, `crates/cdk-fake-wallet/` for the
   test backend. Never grep from `/opt/cashu/corpus/cdk` root when I already know the crate.
4. **Read the tests.** `crates/cdk-integration-tests/` shows working call sequences at this exact
   version — usually faster than reconstructing one from signatures.
5. **Examples are runnable answers.** The `cdk` crate's `examples/` directory has `mint-token.rs`,
   `melt-token.rs`, `receive-token.rs`, `p2pk.rs`, `restore-wallet.rs` and more, all valid at
   0.17.2.

Keep output small: pipe through `head`, and grep for a symbol rather than dumping a file. A file
over ~500 lines gets `sed -n 'START,ENDp'` after I know the line from grep.

## What is NOT here

No rendered rustdoc and no cashudevkit.org mirror. If I want API docs, the source *is* the docs —
doc comments are in the same files. If a question genuinely needs the website, I say so rather than
inventing what it says.
