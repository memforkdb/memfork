## What this changes

<!-- And why. The diff already says what; the useful part is the reason. -->

## How it was tested

<!--
Which of these you ran, and on which operating system:

    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace --all-features
    cargo deny --workspace --all-features check

If you touched the Python package or the installers, say whether you ran
`pytest crates/memfork-py/tests` or `bash installers/test-installer.sh`.
-->

## Anything left out

<!--
Known gaps, decisions you made that could reasonably have gone the other way,
or anything a reviewer should look at twice. "Nothing" is a fine answer.
-->

## Checklist

- [ ] The checks above pass locally
- [ ] New behaviour has a test, and a bug fix has a test that failed before it
- [ ] Documentation updated if the change is user-visible
- [ ] No new dependency, or a new one whose licence fits the policy in `deny.toml`
