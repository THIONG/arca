# Arca contribution guide

This file defines the project-wide rules for agents and contributors. More-specific `AGENTS.md` files override these rules for their directory.

## Project shape

Arca is a cross-platform archiver written in Rust. The workspace contains:

- `arca-core`: shared errors, bounded parsing, limits, and path safety.
- `arca-zip`: ZIP/Zip64 reading and writing, compression, and encryption.
- `arca-tar`: TAR reading and writing.
- `arca-cli`: the `arca` command-line binary.
- `arca-gui`: the desktop window.
- `arca-icons`: desktop file-type icons.
- `arca-drag`: drag-and-drop integration.
- `arca-net`: networking support.
- `windows/arca-shell`: Windows Explorer integration; it is outside the workspace.

Read `README.md` before making broad changes. Design rationale and project history live in `Claude outputs/CLAUDE.md` and `docs/plans/`.

## General rules

- Keep changes focused and minimal. Do not rewrite unrelated code or discard existing working-tree changes.
- Reuse existing helpers, types, and patterns before adding new ones.
- Preserve the safe-Rust boundary: parsers that process untrusted archive bytes must remain free of `unsafe` code.
- Every parser change must include a test for malformed or truncated input and must not panic.
- Keep internal identifiers and source strings ASCII-only. User-facing Spanish text and Markdown documentation must use correct accents.
- Do not add comments to `src/` unless they explain a non-obvious safety or correctness invariant. Build files and CI configuration may include explanatory comments.
- Never publish benchmark numbers without the command needed to reproduce them.

## Rust workflow

Run the narrowest relevant checks while iterating, then run the full checks before handing work over:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release --no-default-features
cargo build --release
bash interop.sh
```

For Windows-only changes, also run the commands from the relevant directory, including `cargo test` and `cargo clippy --release --all-targets -- -D warnings` in `windows/arca-shell` when applicable.

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org/) in **English**.

Format:

```text
<type>(<scope>): <imperative description>
```

Rules:

- Use a lowercase type and scope; omit the scope when it adds no information.
- Write the subject in English, imperative mood, in sentence case, without a trailing period.
- Keep the subject concise (preferably 72 characters or fewer).
- Use the body to explain **why**, not to restate the diff. Write it in English too.
- Add `!` after the type or scope for a breaking change and explain the migration in the body.
- Do not combine unrelated changes in one commit.

Allowed types:

- `feat`: user-visible functionality
- `fix`: bug fix
- `perf`: measurable performance improvement
- `refactor`: behavior-preserving code change
- `test`: tests only
- `docs`: documentation only
- `build`: build system or dependency changes
- `ci`: continuous-integration changes
- `chore`: maintenance that does not fit the categories above
- `revert`: revert a previous commit

Good examples:

```text
feat(arca-cli): add archive password command
fix(arca-zip): reject truncated central directory
perf(arca-gui): avoid rebuilding rows during drag selection
docs: document interoperability checks
```

Pull request titles should follow the same format. The description should state the user-visible or technical outcome, relevant tests, and any platform limitations.

## Documentation and releases

- Put architectural plans in `docs/plans/`.
- Keep `README.md` focused on user-facing usage, supported formats, interoperability, and reproducible checks.
- Keep brand rules in `brand/BRAND.md`.
- Do not tag a release until the tag version matches `Cargo.toml` and CI is green.
