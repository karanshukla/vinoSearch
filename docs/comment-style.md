# Comment style

A comment is the last resort. Work down this ladder, and write prose only
when all four rungs fail.

1. **Extract it.** Logic that a comment explains belongs in a function with
   a domain name. Arithmetic spelled out in a comment belongs in named
   constants. A check that is explained twice belongs in one named helper.

   ```rust
   // Before, in two places
   // Text OCR read in an image has no page or slide to name.
   Kind::Document if !crate::extract::is_document(path) => ...

   // After
   Kind::Document if read_in_an_image(h) => ...
   ```

2. **Name it.** A comment that labels a block (`// The rest of a long one
   waits for index_rest`) means the block wants to be a function
   (`leave_rest_for_later`). Sentinels get names too: write
   `Kind::Media => HELD_BY_DURATION`, not `u64::MAX // held by duration
   instead`. A tuple whose fields need a comment wants to be a struct.

3. **Pin it with a test pair.** A comment that states a business rule is a
   rule nothing enforces. Write one test inside the boundary and one
   outside, named after the rule (`a_file_exactly_at_its_kinds_size_cap_is_indexable`
   / `a_file_one_byte_over_its_kinds_size_cap_is_not_indexable`), instead of
   writing `// exactly at the cap is fine`. The pair is the point: it fails
   the day someone moves the limit. In a test, a comment that labels the
   next few asserts is the same smell. Split those asserts into their own
   test, named after what the comment said. Where the steps share state
   (a recording indexed slice by slice), put the label in the assert's
   message instead, so it shows when the step fails.

4. **Pin cross-boundary rules with an end-to-end test.** What the D-Bus
   interface answers, what `install.sh` writes and the CLI's output belong
   in `tests/` or `index::pipeline_tests`, not in a paragraph.

5. **Only then, prose.** Hidden constraints, upstream bugs, protocol
   requirements, incident history that still constrains the code. State
   the constraint, not its biography: "KRunner counts `code:` towards its
   three letters" is enough, without the issue it came from. Measured
   claims carry a date.

## Linking the test from the code

When rung 3 or 4 replaced a comment, the code it governs says which test
pins the rule, in its doc comment, always with the words "pinned by":

```rust
/// Inside vinoWhisper's folders only sessions are taken: anything else
/// there is a live transcript or a half-written bundle (pinned by
/// `tests::a_live_transcript_in_the_flatpaks_folder_is_not_taken_either`).
```

A test in the same file (or its `tests.rs`) is `tests::name`. Anywhere
else in the crate, give the path from the crate root:
`index::pipeline_tests::name`, `service::worker::tests::name`.

rustdoc can't link to `#[test]` functions (they are compiled out even
under `--cfg test`), so these are code spans, not intra-doc links.
`tests/doc_links.rs` (CI job "Doc Links") checks them instead, in every
crate of the workspace: each cited test must be a `#[test]` fn, and the
comment citing it must say "pinned by". The phrase is one constant there.

## Exempt

- `///` and `//!` docs on items, which say what an item is and how to call
  it. They follow the same "constraint, not biography" rule, and don't
  restate the signature, but aren't replaced by code.
- `// SAFETY:` comments on `unsafe`, `#[allow]` reasons, and coverage
  pragmas.
