# The cg_clif live output sink

`live-sink.patch` adds one output sink to `rustc_codegen_cranelift`: after
`Context::compile` and before the object file, hand the code bytes, their
alignment and their relocations to a callback.

It is 356 added lines, 300 of which are one new file (`src/live_sink.rs`) and
its documentation. The rest is four hooks in `driver/aot.rs`, one `pub(crate)`
on two existing fields in `base.rs`, and one `mod` declaration.

Nothing in codegen changes. With no callback installed the backend behaves
exactly as it did, which is why the pristine source can stay in the toolchain
and only the diff lives here.

    ./build.sh                  # prints the dylib path
    spike --backend <that path> --sequence --fixture fixtures/rg-lib ...

Why a backend patch at all, when §27's closure-only codegen needed none: the
codegen *universe* is a query, and queries can be overridden from outside. The
codegen *output* is not. `CodegenBackend` exposes `codegen_crate` and
`join_codegen`, and the object file is written between them, inside cg_clif.
There is no seam there that is reachable from a driver.
