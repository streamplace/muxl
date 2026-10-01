# Vendored mp4-atom

This directory is a vendored copy of
[streamplace/mp4-atom](https://github.com/streamplace/mp4-atom), branch
`streamplace`, at commit `126c49b8e3e3f8810089ed5cb157da94ce4f04b7` (crate
version 0.10.1). That is the revision muxl previously pinned through its git
dependency. It is licensed MIT OR Apache-2.0; see `LICENSE-MIT` and
`LICENSE-APACHE`.

Only `src/`, `Cargo.toml`, `README.md`, and the license files are vendored.

## Local changes

- **`elng` (ExtendedLanguageBox, ISO/IEC 14496-12 § 8.4.6).** Adds
  `src/moov/trak/mdia/elng.rs`, an `elng: Option<Elng>` field on `Mdia`, and
  an `Any::Elng` variant. Upstream drops unknown `mdia` children, so a BCP 47
  language tag on a WebVTT text track could not round-trip. `Mdia` is now
  hand-written instead of using `nested!`, so that `elng` encodes in the Table 1
  position (`mdhd`, `hdlr`, `elng`, `minf`). When `elng` is `None`, the encoded
  bytes are identical to upstream's.
- **No upstream test fixtures.** `src/test/` (binary MP4 fixtures) is omitted,
  and `mod test` is removed from `src/lib.rs`. Inline unit tests remain.

Root `Cargo.toml` lists this crate in `workspace.exclude`, so it builds only as
a dependency of `muxl-core` and `muxl`.

To drop the vendor, upstream the `elng` change to streamplace/mp4-atom. Then
point both `Cargo.toml` files back at the git dependency and delete this
directory.
